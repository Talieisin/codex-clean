//! Implementations of the `codex-clean seat ...` subcommands.
//!
//! Pure orchestration over `seat.rs`'s data layer. Each function is `pub`
//! and returns `anyhow::Result<()>`; failures bubble up to `main.rs` which
//! prints them and exits non-zero.

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Local, Utc};
use serde_json::json;

use crate::seat::{
    self, codex_auth_path, ensure_file_credential_store, log_event, read_identity, refresh_back_guarded,
    seat_auth_path, seats_dir, swap_active_auth, validate_seat_name, warn_refresh_back,
    CodexLock, FileStoreOutcome, ScratchCodexHome, SeatConfig, SeatEntry,
    SeatIdentity, SeatRuntimeState, SeatState, UsageSnapshot,
};
use crate::runner::{consent_for, CreditUse};
use crate::usage::{
    self, quota_state, AppServerClient, QuotaState, ResetOutcome, UsageClient, UsageFetchError,
};

/// Read lines from a child stdio handle and forward them to our own
/// stdout/stderr, flushing after each line. Solves the case where codex's
/// device-code URL/code would otherwise sit in a stdio buffer for the
/// duration of its OAuth poll when our process's stdout is a pipe.
fn forward_lines_flushing<R: io::Read>(reader: R, to_stdout: bool) {
    use std::io::BufRead;
    let buf = io::BufReader::new(reader);
    for line in buf.lines() {
        let Ok(line) = line else { break };
        if to_stdout {
            let mut out = io::stdout().lock();
            let _ = writeln!(out, "{}", line);
            let _ = out.flush();
        } else {
            let mut err = io::stderr().lock();
            let _ = writeln!(err, "{}", line);
            let _ = err.flush();
        }
    }
}

/// Spawn `codex login [--device-auth]` with `CODEX_HOME` redirected to the
/// given partial directory. Forwards codex's stdio line-by-line with
/// explicit flushes so the device-code URL/code is visible immediately
/// even when this process's stdout is a pipe (CI, Claude Code, etc.).
fn spawn_codex_login_in(home: &Path, browser: bool) -> Result<()> {
    // Seed config.toml in the scratch home so codex login writes to a file
    // (rather than the OS keyring). This is critical: if cli_auth_credentials_store
    // resolves to "keyring", auth.json never appears in our temp home.
    seat::seed_file_store_config(home)?;
    let cfg_path = home.join("config.toml");

    let auth_mode = if browser { "browser" } else { "device-auth" };
    let mut cmd = Command::new("codex");
    cmd.env("CODEX_HOME", home);
    for var in seat::CONSENT_ENV_VARS {
        cmd.env_remove(var);
    }
    cmd.arg("login");
    if !browser {
        cmd.arg("--device-auth");
    }
    cmd.stdin(std::process::Stdio::inherit());
    // Pipe codex's stdout/stderr (not inherit) so we can forward line-by-line
    // with explicit flushes. Without this, the device-code URL/code can sit
    // in codex's stdio buffer for the duration of its OAuth poll when our
    // own stdout is a pipe (e.g. when run from CI, Claude Code, or any
    // non-TTY wrapper) — leaving the user staring at silence.
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning `codex login` ({})", auth_mode))?;

    let child_stdout = child.stdout.take().expect("stdout piped");
    let child_stderr = child.stderr.take().expect("stderr piped");

    let stdout_t = std::thread::spawn(move || forward_lines_flushing(child_stdout, true));
    let stderr_t = std::thread::spawn(move || forward_lines_flushing(child_stderr, false));

    let status = child
        .wait()
        .with_context(|| format!("waiting on `codex login` ({})", auth_mode))?;
    let _ = stdout_t.join();
    let _ = stderr_t.join();

    if !status.success() {
        bail!(
            "`codex login` exited with status {}",
            status.code().unwrap_or(-1)
        );
    }

    let auth = home.join("auth.json");
    if !auth.exists() {
        bail!(
            "`codex login` succeeded but {} is missing — did codex write to the keyring? \
             Check that {} contains cli_auth_credentials_store = \"file\".",
            auth.display(),
            cfg_path.display()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------

pub fn add(name: &str, label: Option<&str>, import: bool, browser: bool) -> Result<()> {
    validate_seat_name(name)?;

    let mut config = SeatConfig::load()?.unwrap_or_default();
    if let Some(existing) = config.find_case_insensitive(name) {
        bail!(
            "seat '{}' already exists{}; remove it first or pick a different name",
            existing.name,
            if existing.name != name { " (names are case-insensitive)" } else { "" }
        );
    }

    let is_first_seat = config.seats.is_empty();
    // Always re-validate so a user who edited config.toml back to "keyring"
    // doesn't silently break multi-seat. report_file_store_outcome stays
    // quiet when the value is already "file".
    let outcome = ensure_file_credential_store()?;
    report_file_store_outcome(outcome);

    if import {
        return add_via_import(name, label, &mut config);
    }

    add_via_login(name, label, browser, &mut config, is_first_seat)
}

fn add_via_import(name: &str, label: Option<&str>, config: &mut SeatConfig) -> Result<()> {
    // Hold the lock for the entire import so a concurrent codex run can't be
    // mid-refresh of ~/.codex/auth.json while we're reading it.
    let _lock = CodexLock::acquire()?;
    let _ = seat::scavenge_scratch_dirs();
    let active_auth = codex_auth_path()?;
    if !active_auth.exists() {
        bail!(
            "cannot --import: {} does not exist (run `codex login` first, or omit --import)",
            active_auth.display()
        );
    }
    let bytes = fs::read(&active_auth)
        .with_context(|| format!("reading {}", active_auth.display()))?;
    // Propagate parse failures (rather than silently importing without an
    // identity and weakening mismatch protection later); missing fields are
    // fine and surface as None.
    let identity = read_identity(&bytes)
        .with_context(|| format!("reading identity from {}", active_auth.display()))?;
    warn_if_identity_incomplete(&identity);
    let dest = seat_auth_path(name)?;
    if let Some(parent) = dest.parent() {
        seat::secure_create_dir_all(parent)?;
    }
    seat::atomic_write(&dest, &bytes)?;

    config.seats.push(SeatEntry {
        name: name.to_string(),
        label: label.map(String::from),
        account_id: identity.account_id,
        user_id: identity.user_id,
    });
    config.save()?;

    let mut state = SeatState::load()?;
    state.active_seat = Some(name.to_string());
    state.save()?;

    eprintln!("Imported existing ~/.codex/auth.json as seat '{}'.", name);
    Ok(())
}

fn add_via_login(
    name: &str,
    label: Option<&str>,
    browser: bool,
    config: &mut SeatConfig,
    is_first_seat: bool,
) -> Result<()> {
    let _lock = CodexLock::acquire()?;
    let _ = seat::scavenge_scratch_dirs();

    eprintln!(
        "Starting login for seat '{}'. The codex CLI will print a URL and code below — open the URL in any browser, sign in to the {}ChatGPT account for this seat, and enter the code.",
        name,
        if is_first_seat { "" } else { "second " }
    );

    // Run codex login against an isolated temp CODEX_HOME so the active
    // ~/.codex/auth.json is never replaced. Ctrl-C in the middle just leaves
    // the scratch dir, which the guard cleans up on drop (or scavenging
    // later, if the process was killed outright).
    let scratch = ScratchCodexHome::create_for(name, "partial")?;
    spawn_codex_login_in(scratch.path(), browser)?;

    let temp_auth = scratch.auth_path();
    let auth_bytes = fs::read(&temp_auth)
        .with_context(|| format!("reading {}", temp_auth.display()))?;
    let identity = read_identity(&auth_bytes)
        .with_context(|| format!("parsing {}", temp_auth.display()))?;
    warn_if_identity_incomplete(&identity);

    let dest = seat_auth_path(name)?;
    if let Some(parent) = dest.parent() {
        seat::secure_create_dir_all(parent)?;
    }
    seat::atomic_write(&dest, &auth_bytes)?;
    drop(scratch);

    config.seats.push(SeatEntry {
        name: name.to_string(),
        label: label.map(String::from),
        account_id: identity.account_id,
        user_id: identity.user_id,
    });
    config.save()?;

    // Deliberately not recorded as active: ~/.codex/auth.json still holds
    // whatever the user was logged in as, and recording a seat as active
    // whose blob is not in the global file would make the next run's
    // refresh-back copy the wrong blob into this slot. `seat use` (or the
    // first rotation) swaps it in properly.
    let _ = is_first_seat;
    eprintln!(
        "Seat '{}' added. Run `codex-clean seat use {}` to make it active now, or just run \
         codex-clean and let rotation pick it.",
        name, name
    );
    Ok(())
}

fn warn_if_identity_incomplete(identity: &SeatIdentity) {
    if identity.account_id.is_none() || identity.user_id.is_none() {
        eprintln!(
            "Warning: could not extract a full identity ({}) from the new auth.json; \
             account-mismatch protection on refresh-back and `seat login` will be \
             unavailable for this seat.",
            identity
        );
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum LoginIdentityCheck {
    Ok,
    /// The new blob lacks a claim the seat has on record.
    MissingClaims,
    /// A recorded claim is present in the new blob but differs.
    Mismatch,
}

/// Compare a fresh login against a seat's recorded identity. Recorded claims
/// are mandatory in the new blob; unrecorded ones are not checked.
pub fn verify_login_identity(expected: &SeatIdentity, got: &SeatIdentity) -> LoginIdentityCheck {
    let mut missing = false;
    let mut mismatch = false;
    for (e, g) in [
        (&expected.account_id, &got.account_id),
        (&expected.user_id, &got.user_id),
    ] {
        match (e, g) {
            (Some(_), None) => missing = true,
            (Some(e), Some(g)) if e != g => mismatch = true,
            _ => {}
        }
    }
    if mismatch {
        LoginIdentityCheck::Mismatch
    } else if missing {
        LoginIdentityCheck::MissingClaims
    } else {
        LoginIdentityCheck::Ok
    }
}

fn report_file_store_outcome(outcome: FileStoreOutcome) {
    match outcome {
        FileStoreOutcome::AlreadyFile => {}
        FileStoreOutcome::Added => {
            eprintln!(
                "Set cli_auth_credentials_store = \"file\" in ~/.codex/config.toml (required for multi-seat to work)."
            );
        }
        FileStoreOutcome::Changed { previous } => {
            eprintln!(
                "Changed cli_auth_credentials_store from \"{}\" to \"file\" in ~/.codex/config.toml.",
                previous
            );
            if previous == "keyring" {
                eprintln!(
                    "Note: any tokens previously stored in the OS keyring are now invisible to codex. You may need to re-run `codex login` for the existing seat."
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

pub fn list() -> Result<()> {
    let config = match SeatConfig::load()? {
        Some(c) => c,
        None => {
            eprintln!("No seats configured. Run `codex-clean seat add <name> --import` to start.");
            return Ok(());
        }
    };
    if config.seats.is_empty() {
        eprintln!("No seats configured.");
        return Ok(());
    }

    let state = SeatState::load()?;
    let now = Utc::now();
    let active = state.active_seat.as_deref();

    println!(
        "{:<14} {:<22} {:<17} {:<25} {:<10} {:<22}",
        "NAME", "LABEL", "LAST USED", "USAGE", "FETCHED", "STATUS"
    );
    for seat in &config.seats {
        let st = state.get(&seat.name);
        let label = seat.label.as_deref().unwrap_or("-");
        let last_used = match st.last_used {
            Some(t) => format_local(t),
            None => "never".to_string(),
        };
        let (usage_col, fetched_col) = match &st.usage {
            Some(u) => (
                format!(
                    "{}{}",
                    usage::summarize_usage_short(u),
                    if usage::credits_available(u) { " +credits" } else { "" }
                ),
                format!("{} ago", usage::format_duration_short(now - u.fetched_at)),
            ),
            None => ("-".to_string(), "-".to_string()),
        };
        let status = format_status(
            &st,
            active.map(|a| a == seat.name).unwrap_or(false),
            now,
            consent_for(&config, &state, &seat.name, &[], now),
            config.rotation.credits,
        );
        println!(
            "{:<14} {:<22} {:<17} {:<25} {:<10} {:<22}",
            seat.name,
            truncate(label, 22),
            last_used,
            truncate(&usage_col, 25),
            fetched_col,
            status
        );
    }
    Ok(())
}

fn format_status(
    st: &SeatRuntimeState,
    is_active: bool,
    now: DateTime<Utc>,
    consent: Option<CreditUse>,
    policy: seat::CreditPolicy,
) -> String {
    if st.needs_login {
        return "needs login".to_string();
    }
    if let Some(until) = st.cooldown_until {
        if until > now {
            let reason = st
                .cooldown_reason
                .as_deref()
                .filter(|r| *r != "rate_limit")
                .map(|r| format!(" ({})", r))
                .unwrap_or_default();
            return format!(
                "cooling{} until {}",
                reason,
                until.with_timezone(&Local).format("%-I:%M %p")
            );
        }
    }
    if let QuotaState::OnCredits { .. } = quota_state(st, now) {
        return match (consent, is_active) {
            (Some(_), true) => "ready (active, on credits)".to_string(),
            (Some(_), false) => "ready (on credits)".to_string(),
            (None, _) => format!("quota used; credits not in use ({})", policy),
        };
    }
    if is_active {
        "ready (active)".to_string()
    } else {
        "ready".to_string()
    }
}

// ---------------------------------------------------------------------------
// status (live usage via codex app-server)
// ---------------------------------------------------------------------------

/// `codex-clean seat status [NAME] [--json] [--clear-cooldown NAME]`.
pub fn status(
    name: Option<&str>,
    json_out: bool,
    clear_cooldown: Option<&str>,
    with_usage: bool,
) -> Result<i32> {
    status_with_opts(&AppServerClient::default(), name, json_out, clear_cooldown, with_usage)
}

/// Outcome for one seat, assembled for both the table and `--json`.
struct SeatStatusRow {
    name: String,
    label: Option<String>,
    active: bool,
    state: SeatRuntimeState,
    result: Result<UsageSnapshot, UsageFetchError>,
    notices: Vec<String>,
    quota: QuotaState,
    consent: Option<CreditUse>,
    policy: seat::CreditPolicy,
}

/// Injectable core of `status`. Returns the process exit code.
pub fn status_with(
    client: &dyn UsageClient,
    only: Option<&str>,
    json_out: bool,
    clear_cooldown: Option<&str>,
) -> Result<i32> {
    status_with_opts(client, only, json_out, clear_cooldown, false)
}

/// `status_with`, plus the opt-in account-usage lines.
pub fn status_with_opts(
    client: &dyn UsageClient,
    only: Option<&str>,
    json_out: bool,
    clear_cooldown: Option<&str>,
    with_usage: bool,
) -> Result<i32> {
    // Lock first, then load: a concurrent add/remove/run cannot leave us
    // with stale config, and — more importantly — a codex-clean run's own
    // refresh-back cannot interleave with the token sync we do below.
    let Some(_lock) = CodexLock::try_acquire()? else {
        bail!(
            "a codex-clean run is in progress (holding {}); \
             use `codex-clean seat list` for the cached snapshot and retry later",
            seat::lock_path()?.display()
        );
    };
    let _ = seat::scavenge_scratch_dirs();

    let config = match SeatConfig::load()? {
        Some(c) if !c.seats.is_empty() => c,
        _ => {
            eprintln!("No seats configured. Run `codex-clean seat add <name> --import` to start.");
            return Ok(0);
        }
    };
    if let Some(n) = only {
        if config.find(n).is_none() {
            bail!("seat '{}' not found; run `codex-clean seat list` to see configured seats", n);
        }
    }
    if let Some(n) = clear_cooldown {
        if config.find(n).is_none() {
            bail!("--clear-cooldown: seat '{}' not found", n);
        }
    }
    let mut state = SeatState::load()?;
    let mut global_notices: Vec<String> = Vec::new();

    // --clear-cooldown is independent of which seats are fetched, so
    // `seat status b --clear-cooldown a` still clears a.
    if let Some(n) = clear_cooldown {
        let entry = state.entry_mut(n);
        if entry.cooldown_until.is_some() {
            entry.cooldown_until = None;
            entry.cooldown_reason = None;
            global_notices.push(format!("cleared cooldown for seat '{}'", n));
        } else {
            global_notices.push(format!("seat '{}' had no cooldown to clear", n));
        }
    }

    // Sync 1: the active seat's slot may be behind ~/.codex/auth.json (a
    // plain `codex` session refreshed it). Bring the slot up to date so the
    // scratch copy starts from the freshest tokens.
    let active = state.active_seat.clone();
    let mut active_slot_before: Option<Vec<u8>> = None;
    if let Some(a) = active.as_deref() {
        match refresh_back_guarded(a, &config.identity_for(a)) {
            Ok(outcome) => {
                warn_refresh_back(a, &outcome);
                if let seat::RefreshBackOutcome::Copied = outcome {
                    global_notices.push(format!(
                        "synced a token refresh from ~/.codex/auth.json into seat '{}'",
                        a
                    ));
                }
            }
            Err(e) => eprintln!("Warning: refresh-back for active seat '{}' failed: {:#}", a, e),
        }
        active_slot_before = fs::read(seat_auth_path(a)?).ok();
    }

    let targets: Vec<SeatEntry> = config
        .seats
        .iter()
        .filter(|s| only.is_none_or(|n| n == s.name))
        .cloned()
        .collect();
    let results = usage::fetch_all(client, &targets);

    let now = Utc::now();
    // Reconcile the whole batch first (order-independent; blockers dominate),
    // then build every row from the final state so the table, --json and
    // state.json always agree.
    let mut fetched = Vec::new();
    for (name, result) in &results {
        state.entry_mut(name).usage_checked_at = Some(now);
        match result {
            Ok(snap) => fetched.push((name.clone(), snap.clone())),
            Err(UsageFetchError::AuthRequired) => {
                state.entry_mut(name).needs_login = true;
                log_event("auth_error", name, "usage check rejected the seat's tokens; marked needs_login");
            }
            Err(e) => log_event("status_error", name, &e.to_string()),
        }
    }
    let mut per_seat: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for (seat_name, notice) in usage::reconcile_snapshots(&config, &mut state, fetched, now) {
        log_event("status", &seat_name, &notice);
        per_seat.entry(seat_name).or_default().push(notice);
    }
    state.save()?;
    let mut rows = Vec::with_capacity(results.len());
    for (seat_entry, (_, result)) in targets.iter().zip(results) {
        let st = state.get(&seat_entry.name);
        rows.push(SeatStatusRow {
            name: seat_entry.name.clone(),
            label: seat_entry.label.clone(),
            active: active.as_deref() == Some(seat_entry.name.as_str()),
            quota: quota_state(&st, now),
            consent: consent_for(&config, &state, &seat_entry.name, &[], now),
            policy: config.rotation.credits,
            state: st,
            result,
            notices: per_seat.remove(&seat_entry.name).unwrap_or_default(),
        });
    }
    // Notices about seats that were not fetched (workspace propagation).
    for (_, v) in per_seat {
        global_notices.extend(v);
    }
    if with_usage {
        for seat_entry in &targets {
            let before = seat::slot_snapshot(&seat_entry.name);
            let got = client.account_usage(seat_entry);
            seat::sync_active_auth(&seat_entry.name, before);
            match got {
                Ok(u) => global_notices.push(format!(
                    "{}: {} tokens in the last 7 days{}",
                    seat_entry.name,
                    usage::format_tokens(u.last_7d_tokens),
                    u.lifetime_tokens
                        .map(|t| format!(", {} lifetime", usage::format_tokens(t)))
                        .unwrap_or_default()
                )),
                Err(e) => global_notices.push(format!("{}: account usage unavailable ({})", seat_entry.name, e)),
            }
        }
    }
    global_notices.insert(0, describe_credit_policy(&config, &state, now));

    // Sync 2: if the app-server rotated the active seat's token, push the new
    // blob into ~/.codex/auth.json so plain codex does not keep using an
    // invalidated refresh token. We hold the lock; a concurrently running
    // plain `codex` session is documented as unsupported.
    if let Some(a) = active.as_deref() {
        let after = fs::read(seat_auth_path(a)?).ok();
        if after.is_some() && after != active_slot_before {
            match swap_active_auth(a) {
                Ok(()) => global_notices.push(format!(
                    "seat '{}' refreshed its token during the check; ~/.codex/auth.json updated",
                    a
                )),
                Err(e) => eprintln!(
                    "Warning: could not update ~/.codex/auth.json with seat '{}''s refreshed token: {:#}",
                    a, e
                ),
            }
        }
    }

    let any_ok = rows.iter().any(|r| r.result.is_ok());
    if json_out {
        print_status_json(&rows, &global_notices, &config, &state, now)?;
    } else {
        print_status_table(&rows, &global_notices, now);
    }
    Ok(if any_ok { 0 } else { 1 })
}

fn print_status_table(rows: &[SeatStatusRow], global_notices: &[String], now: DateTime<Utc>) {
    const W: (usize, usize, usize, usize, usize) = (14, 18, 8, 28, 28);
    const WC: usize = 14;
    const WR: usize = 7;
    println!(
        "{:<w0$} {:<w1$} {:<w2$} {:<w3$} {:<w4$} {:<wc$} {:<wr$} STATUS",
        "NAME",
        "LABEL",
        "PLAN",
        "5H",
        "WEEKLY",
        "CREDITS",
        "RESETS",
        w0 = W.0,
        w1 = W.1,
        w2 = W.2,
        w3 = W.3,
        w4 = W.4,
        wc = WC,
        wr = WR
    );
    let mut footnotes: Vec<String> = Vec::new();
    for row in rows {
        let label = row.label.as_deref().unwrap_or("-");
        let (plan, five_h, weekly) = match &row.result {
            Ok(snap) => {
                let plan = snap.plan_type.clone().unwrap_or_else(|| "-".to_string());
                let bucket = usage::primary_bucket(snap);
                let cell = |minutes: u64| {
                    bucket
                        .and_then(|b| usage::find_window(b, minutes))
                        .map(|w| usage::format_window_cell(w, now))
                        .unwrap_or_else(|| "-".to_string())
                };
                // Anything outside the two headline windows goes in a footnote
                // so nothing is silently dropped.
                for b in &snap.buckets {
                    let is_primary = bucket.is_some_and(|p| std::ptr::eq(p, b));
                    for w in &b.windows {
                        let headline = is_primary
                            && matches!(
                                w.window_minutes,
                                Some(usage::FIVE_HOUR_MINUTES) | Some(usage::WEEKLY_MINUTES)
                            );
                        if !headline {
                            footnotes.push(format!(
                                "  {}: {} {} {}",
                                row.name,
                                b.limit_id.as_deref().unwrap_or("limit"),
                                usage::window_label(w.window_minutes),
                                usage::format_window_cell(w, now)
                            ));
                        }
                    }
                    if let Some(kind) = &b.rate_limit_reached_type {
                        footnotes.push(format!("  {}: backend reports {}", row.name, kind));
                    }
                }
                if snap.spend_control_reached == Some(true) {
                    footnotes.push(format!("  {}: workspace spend cap reached", row.name));
                }
                (plan, cell(usage::FIVE_HOUR_MINUTES), cell(usage::WEEKLY_MINUTES))
            }
            Err(_) => ("?".to_string(), "?".to_string(), "?".to_string()),
        };
        let status = format_status(&row.state, row.active, now, row.consent, row.policy);
        let credits = match &row.result {
            Ok(snap) => usage::format_credits(snap),
            Err(_) => "?".to_string(),
        };
        let resets = match &row.result {
            Ok(snap) => snap
                .resets
                .as_ref()
                .map(|r| r.available.to_string())
                .unwrap_or_else(|| "-".to_string()),
            Err(_) => "?".to_string(),
        };
        println!(
            "{:<w0$} {:<w1$} {:<w2$} {:<w3$} {:<w4$} {:<wc$} {:<wr$} {}",
            row.name,
            truncate(label, W.1),
            truncate(&plan, W.2),
            five_h,
            weekly,
            truncate(&credits, WC),
            resets,
            status,
            w0 = W.0,
            w1 = W.1,
            w2 = W.2,
            w3 = W.3,
            w4 = W.4,
            wc = WC,
            wr = WR
        );
    }
    if !footnotes.is_empty() {
        println!();
        for f in footnotes {
            println!("{}", f);
        }
    }
    let mut any = false;
    for row in rows {
        if let Err(e) = &row.result {
            if !any {
                println!();
                any = true;
            }
            let hint = match e {
                UsageFetchError::AuthRequired => {
                    format!(" — run `codex-clean seat login {}`", row.name)
                }
                _ => String::new(),
            };
            println!("! {}: {}{}", row.name, e, hint);
        }
    }
    let notices: Vec<&String> = global_notices
        .iter()
        .chain(rows.iter().flat_map(|r| r.notices.iter()))
        .collect();
    if !notices.is_empty() {
        println!();
        for n in notices {
            println!("• {}", n);
        }
    }
}

fn print_status_json(
    rows: &[SeatStatusRow],
    global_notices: &[String],
    config: &SeatConfig,
    state: &SeatState,
    now: DateTime<Utc>,
) -> Result<()> {
    let seats: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let (usage_v, error_v) = match &r.result {
                Ok(snap) => (serde_json::to_value(snap).unwrap_or(json!(null)), json!(null)),
                Err(e) => (json!(null), json!(e.to_string())),
            };
            let quota_resets = match r.quota {
                QuotaState::OnCredits { resets_at } => json!(resets_at),
                _ => json!(null),
            };
            json!({
                "name": r.name,
                "label": r.label,
                "active": r.active,
                "needs_login": r.state.needs_login,
                "cooldown_until": r.state.cooldown_until,
                "cooldown_reason": r.state.cooldown_reason,
                "quota_state": { "state": r.quota.as_str(), "resets_at": quota_resets },
                "free_resets": r.result.as_ref().ok().and_then(|s| s.resets.as_ref()).map(|r| json!({
                    "available": r.available,
                    "next_expires_at": r.next_expires_at,
                    "next_title": r.next_title,
                })),
                "usage": usage_v,
                "error": error_v,
                "notices": r.notices,
            })
        })
        .collect();
    let doc = json!({
        "credits_mode": config.rotation.credits.as_str(),
        "resets_mode": config.rotation.resets.as_str(),
        "credit_grants": active_grants_json(config, state, now),
        "seats": seats,
        "notices": global_notices,
    });
    println!("{}", serde_json::to_string_pretty(&doc)?);
    Ok(())
}

/// Active grants, reported by the seat names they cover (account ids stay
/// out of `--json`). Expired grants are omitted.
fn active_grants_json(config: &SeatConfig, state: &SeatState, now: DateTime<Utc>) -> serde_json::Value {
    let grants: Vec<serde_json::Value> = active_grants(config, state, now)
        .into_iter()
        .map(|(workspace, seats, until)| json!({ "workspace": workspace, "seats": seats, "until": until }))
        .collect();
    json!(grants)
}

/// Active grants as `(opaque workspace label, seat names, until)`.
fn active_grants(
    config: &SeatConfig,
    state: &SeatState,
    now: DateTime<Utc>,
) -> Vec<(String, Vec<String>, DateTime<Utc>)> {
    state
        .credit_grants
        .iter()
        .filter(|(_, until)| **until > now)
        .map(|(ws, until)| {
            let seats: Vec<String> = config
                .seats
                .iter()
                .filter(|s| seat::workspace_key(config, &s.name) == *ws)
                .map(|s| s.name.clone())
                .collect();
            (seat::workspace_label(ws), seats, *until)
        })
        .filter(|(_, seats, _)| !seats.is_empty())
        .collect()
}

/// "credits: ask (no active grant)" / "credits: ask; granted until Thu 08:09
/// for main, backup1" / "credits: always".
fn describe_credit_policy(config: &SeatConfig, state: &SeatState, now: DateTime<Utc>) -> String {
    let mut s = format!("credits: {}", config.rotation.credits);
    if config.rotation.credits != seat::CreditPolicy::Always {
        let grants = active_grants(config, state, now);
        if grants.is_empty() {
            s.push_str(" (no active grant)");
        } else {
            for (_, seats, until) in grants {
                s.push_str(&format!(
                    "; granted until {} for {}",
                    until.with_timezone(&Local).format("%a %H:%M"),
                    seats.join(", ")
                ));
            }
        }
        if seat::env_use_credits() {
            s.push_str("; CODEX_CLEAN_USE_CREDITS=1 is set");
        }
    }
    s
}

// ---------------------------------------------------------------------------
// credits
// ---------------------------------------------------------------------------

/// `codex-clean seat credits [ask|never|always|allow|revoke]`.
pub fn credits(action: Option<&str>) -> Result<()> {
    let Some(action) = action else {
        let now = Utc::now();
        let config = SeatConfig::load()?
            .ok_or_else(|| anyhow!("no seats configured; run `codex-clean seat add <name>` first"))?;
        let state = SeatState::load()?;
        println!("{}", describe_credit_policy(&config, &state, now));
        for s in &config.seats {
            let st = state.get(&s.name);
            let credits = st.usage.as_ref().map(usage::format_credits).unwrap_or_else(|| "-".into());
            let quota = match quota_state(&st, now) {
                QuotaState::OnCredits { resets_at } => format!(
                    "included quota used{}",
                    resets_at
                        .map(|r| format!(" (resets {})", r.with_timezone(&Local).format("%a %H:%M")))
                        .unwrap_or_default()
                ),
                QuotaState::Within => "within included quota".to_string(),
                QuotaState::Unknown => "no usage recorded yet".to_string(),
            };
            let free = st
                .usage
                .as_ref()
                .and_then(|u| u.resets.as_ref())
                .filter(|r| r.available > 0)
                .map(|r| {
                    format!(
                        "; {} free usage-limit reset(s){}",
                        r.available,
                        r.next_expires_at
                            .map(|e| format!(" (next expires {})", e.with_timezone(&Local).format("%a %d %b")))
                            .unwrap_or_default()
                    )
                })
                .unwrap_or_default();
            println!("  {}: credits {}; {}{}", s.name, credits, quota, free);
        }
        println!("Change with: codex-clean seat credits ask|never|always|allow|revoke");
        return Ok(());
    };

    let _lock = CodexLock::acquire()?;
    // Time is read after the lock: waiting for it can cross a quota reset.
    let now = Utc::now();
    let mut config = SeatConfig::load()?
        .ok_or_else(|| anyhow!("no seats configured; run `codex-clean seat add <name>` first"))?;
    let mut state = SeatState::load()?;
    match action.trim().to_ascii_lowercase().as_str() {
        "allow" => {
            let on_credits: Vec<(String, Option<DateTime<Utc>>)> = config
                .seats
                .iter()
                .filter_map(|s| match quota_state(&state.get(&s.name), now) {
                    QuotaState::OnCredits { resets_at } => Some((s.name.clone(), resets_at)),
                    _ => None,
                })
                .collect();
            if on_credits.is_empty() {
                bail!(
                    "nothing to allow yet: no seat has used up its included quota with credits available \
                     (according to the last `codex-clean seat status`)"
                );
            }
            let granted = seat::grant_credits_until_reset(&config, &mut state, &on_credits, now)?;
            state.save()?;
            for (seats, until) in granted {
                log_event("credits_grant", &seats.join(","), &format!("until {}", until.to_rfc3339()));
                eprintln!(
                    "Credits allowed for {} until {} (when included quota resets).",
                    seats.join(", "),
                    until.with_timezone(&Local).format("%a %H:%M")
                );
            }
        }
        "revoke" => {
            let n = state.credit_grants.len();
            state.credit_grants.clear();
            state.save()?;
            log_event("credits_grant", "-", "revoked all grants");
            eprintln!("Revoked {} credit grant(s).", n);
        }
        other => {
            let policy = seat::CreditPolicy::parse(other).ok_or_else(|| {
                anyhow!("unknown credits action '{}'; use ask, never, always, allow or revoke", other)
            })?;
            config.rotation.credits = policy;
            config.save()?;
            log_event("credits_mode", "-", &format!("set to {}", policy));
            eprintln!("Credits mode is now {}.", policy);
        }
    }
    Ok(())
}

fn format_local(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}

// ---------------------------------------------------------------------------
// login (re-auth existing seat)
// ---------------------------------------------------------------------------

pub fn login(name: &str, browser: bool) -> Result<()> {
    let mut config = SeatConfig::load()?
        .ok_or_else(|| anyhow!("no seats configured; run `codex-clean seat add <name>` first"))?;
    let expected = config
        .find(name)
        .ok_or_else(|| {
            anyhow!(
                "seat '{}' not found; run `codex-clean seat list` to see configured seats",
                name
            )
        })?
        .identity();

    let _lock = CodexLock::acquire()?;
    let _ = seat::scavenge_scratch_dirs();
    // Re-validate config.toml every login in case the user (or some other
    // tool) flipped cli_auth_credentials_store back to keyring.
    let outcome = ensure_file_credential_store()?;
    report_file_store_outcome(outcome);

    eprintln!(
        "Re-authenticating seat '{}'. Sign in as the SAME ChatGPT account when prompted.",
        name
    );

    // Run codex login against an isolated temp CODEX_HOME so a Ctrl-C or a
    // wrong-account login can't damage ~/.codex/auth.json.
    let scratch = ScratchCodexHome::create_for(name, "partial")?;
    spawn_codex_login_in(scratch.path(), browser)?;

    let temp_auth = scratch.auth_path();
    let new_auth = fs::read(&temp_auth)
        .with_context(|| format!("reading {}", temp_auth.display()))?;
    let got = read_identity(&new_auth).with_context(|| format!("parsing {}", temp_auth.display()))?;

    // Identity verification. Every claim the seat has on record must be
    // present in the new blob and equal; a claim the seat never recorded
    // (legacy entry) is adopted below. Two seats in one Team workspace share
    // an account_id, so the user claim is what catches signing in as the
    // wrong colleague — and a blob that *lacks* the user claim is refused
    // rather than waved through.
    match verify_login_identity(&expected, &got) {
        LoginIdentityCheck::Ok => {}
        LoginIdentityCheck::MissingClaims => bail!(
            "The new auth.json lacks an identity claim that seat '{}' has on record ({}); \
             got {}. Refusing to overwrite — we cannot prove it is the same user.",
            name,
            expected,
            got
        ),
        LoginIdentityCheck::Mismatch => bail!(
            "Identity mismatch: seat '{}' was registered as {} but you signed in as {}. \
             The existing tokens were left untouched. \
             If you genuinely want to repoint this seat, remove and re-add it: \
             `codex-clean seat remove {} && codex-clean seat add {}`.",
            name,
            expected,
            got,
            name,
            name
        ),
    }
    if expected.account_id.is_none() && expected.user_id.is_none() {
        warn_if_identity_incomplete(&got);
    }

    let dest = seat_auth_path(name)?;
    if let Some(parent) = dest.parent() {
        seat::secure_create_dir_all(parent)?;
    }
    seat::atomic_write(&dest, &new_auth)?;
    drop(scratch);

    // Adopt any identity fields the seat didn't have stored yet (e.g. it was
    // added before `user_id` existed).
    if let Some(seat_entry) = config.seats.iter_mut().find(|s| s.name == name) {
        if seat_entry.account_id.is_none() {
            seat_entry.account_id = got.account_id.clone();
        }
        if seat_entry.user_id.is_none() {
            seat_entry.user_id = got.user_id.clone();
        }
    }

    let mut state = SeatState::load()?;
    let entry = state.entry_mut(name);
    entry.needs_login = false;
    entry.consecutive_failures = 0;
    state.save()?;

    // If this seat is the active one, ~/.codex/auth.json still holds its OLD
    // tokens. Left alone, the next run's refresh-back would see "same
    // identity, different bytes" and copy the old blob back over the login
    // we just saved. Keep global and slot in step (we hold the lock).
    if state.active_seat.as_deref() == Some(name) {
        swap_active_auth(name)
            .with_context(|| format!("installing seat '{}''s new login into ~/.codex/auth.json", name))?;
    }
    log_event("login", name, &format!("re-authenticated as {}", got));

    // Persist any identity fields we may have just adopted. Failing here would
    // weaken mismatch protection on future re-logins, so propagate.
    config
        .save()
        .with_context(|| format!("saving updated seat config for '{}'", name))?;
    eprintln!("Seat '{}' re-authenticated.", name);
    Ok(())
}

// ---------------------------------------------------------------------------
// use
// ---------------------------------------------------------------------------

pub fn use_seat(name: &str) -> Result<()> {
    let config = SeatConfig::load()?
        .ok_or_else(|| anyhow!("no seats configured; run `codex-clean seat add <name>` first"))?;
    if config.find(name).is_none() {
        bail!("seat '{}' not found", name);
    }

    let _lock = CodexLock::acquire()?;

    // Capture any token refreshes codex may have written into the active
    // ~/.codex/auth.json BEFORE we overwrite it. Doing this after the swap
    // would clobber the previous seat's slot with the new seat's blob.
    let mut state = SeatState::load()?;
    let prev_active = state.active_seat.clone();
    if let Some(prev) = prev_active.as_deref() {
        // Includes prev == name: a fresher global blob must not be clobbered
        // by a stale slot copy either way.
        let outcome = refresh_back_guarded(prev, &config.identity_for(prev))
            .with_context(|| format!("refresh-back for previously active seat '{}'", prev))?;
        warn_refresh_back(prev, &outcome);
    }

    swap_active_auth(name)?;
    state.active_seat = Some(name.to_string());
    state.save()?;
    log_event("use", name, "made active");
    eprintln!("Active seat is now '{}'.", name);
    Ok(())
}

// ---------------------------------------------------------------------------
// strategy
// ---------------------------------------------------------------------------

/// `codex-clean seat strategy [NAME [SEAT]]`: show or set the rotation
/// strategy in seats.toml.
pub fn strategy(name: Option<&str>, fixed_seat: Option<&str>) -> Result<()> {
    // Config writers serialise on the lock so a concurrent add/login/remove
    // cannot be overwritten from a stale in-memory copy.
    let _lock = if name.is_some() { Some(CodexLock::acquire()?) } else { None };
    let mut config = SeatConfig::load()?
        .ok_or_else(|| anyhow!("no seats configured; run `codex-clean seat add <name>` first"))?;
    let Some(name) = name else {
        let extra = match config.rotation.strategy {
            seat::Strategy::Fixed => format!(
                " (fixed_seat = {})",
                config.rotation.fixed_seat.as_deref().unwrap_or("?")
            ),
            seat::Strategy::Balanced => format!(
                " (balance_refresh_seconds = {})",
                config.rotation.balance_refresh_seconds
            ),
            _ => String::new(),
        };
        println!("{}{}", config.rotation.strategy, extra);
        println!("Available: least-recently-used (lru), round-robin (rr), fixed <seat>, balanced");
        return Ok(());
    };
    let strategy = seat::Strategy::parse(name).ok_or_else(|| {
        anyhow!(
            "unknown strategy '{}'; use least-recently-used (lru), round-robin (rr), fixed <seat>, or balanced",
            name
        )
    })?;
    if strategy == seat::Strategy::Fixed {
        let seat_name = fixed_seat.ok_or_else(|| anyhow!("`fixed` needs a seat: codex-clean seat strategy fixed <seat>"))?;
        if config.find(seat_name).is_none() {
            bail!("seat '{}' not found; run `codex-clean seat list` to see configured seats", seat_name);
        }
        config.rotation.fixed_seat = Some(seat_name.to_string());
    } else if fixed_seat.is_some() {
        bail!("a seat argument only applies to the `fixed` strategy");
    }
    config.rotation.strategy = strategy;
    config.validate()?;
    config.save()?;
    let event_seat = if strategy == seat::Strategy::Fixed {
        config.rotation.fixed_seat.as_deref().unwrap_or("-")
    } else {
        "-"
    };
    log_event("strategy", event_seat, &format!("set to {}", strategy));
    eprintln!(
        "Rotation strategy is now {}{}.",
        strategy,
        config
            .rotation
            .fixed_seat
            .as_deref()
            .filter(|_| strategy == seat::Strategy::Fixed)
            .map(|s| format!(" (preferring seat '{}')", s))
            .unwrap_or_default()
    );
    Ok(())
}

/// `codex-clean seat reset-policy [ask|never|auto]`: show or set whether a
/// blocked run may redeem a free usage-limit reset.
///
/// Named `reset-policy` rather than `resets` on purpose: `seat reset` redeems a
/// finite grant immediately, so a dropped "s" on `seat resets never` would spend
/// one. This name cannot be fat-fingered into a redemption.
pub fn reset_policy(action: Option<&str>) -> Result<()> {
    let _lock = if action.is_some() { Some(CodexLock::acquire()?) } else { None };
    let mut config = SeatConfig::load()?
        .ok_or_else(|| anyhow!("no seats configured; run `codex-clean seat add <name>` first"))?;
    let Some(action) = action else {
        println!("{}", config.rotation.resets);
        println!("Available: ask (default), never, auto");
        return Ok(());
    };
    let policy = seat::ResetPolicy::parse(action)
        .ok_or_else(|| anyhow!("unknown reset policy '{}'; use ask, never or auto", action))?;
    config.rotation.resets = policy;
    config.validate()?;
    config.save()?;
    log_event("reset_policy", "-", &format!("set to {}", policy));
    eprintln!("Free-reset policy is now {}.", policy);
    Ok(())
}

// ---------------------------------------------------------------------------
// reset (redeem a free usage-limit reset)
// ---------------------------------------------------------------------------

/// `codex-clean seat reset [NAME] [--credit-id ID] [--dry-run] [--json]`.
pub fn reset(
    name: Option<&str>,
    credit_id: Option<&str>,
    dry_run: bool,
    json_out: bool,
) -> Result<i32> {
    reset_with(&AppServerClient::default(), name, credit_id, dry_run, json_out)
}

/// Injectable core of `seat reset`.
pub fn reset_with(
    client: &dyn UsageClient,
    name: Option<&str>,
    credit_id: Option<&str>,
    dry_run: bool,
    json_out: bool,
) -> Result<i32> {
    let _ = seat::scavenge_scratch_dirs();
    let Some(_lock) = CodexLock::try_acquire()? else {
        bail!(
            "a codex-clean run is in progress (holding {}); retry when it finishes",
            seat::lock_path()?.display()
        );
    };
    let config = SeatConfig::load()?
        .filter(|c| !c.seats.is_empty())
        .ok_or_else(|| anyhow!("no seats configured; run `codex-clean seat add <name>` first"))?;
    let mut state = SeatState::load()?;
    let now = Utc::now();
    // Default: the seat a run would pick, else the active one, else the first.
    let seat_name = match name {
        Some(n) => config
            .find(n)
            .map(|s| s.name.clone())
            .ok_or_else(|| anyhow!("seat '{}' not found; run `codex-clean seat list`", n))?,
        None => {
            // The seat a run would actually use: honour a pin, then prefer a
            // seat a reset would unblock (the normal exit-78 case, where
            // ordinary picking fails), then the seat a run would pick.
            let pinned = std::env::var("CODEX_CLEAN_SEAT").ok().filter(|s| !s.is_empty());
            let eligible = seat::reset_eligible_seats(&config, &state, now);
            pinned
                .filter(|p| config.find(p).is_some())
                .or_else(|| eligible.first().map(|(n, _, _)| n.clone()))
                .or_else(|| seat::pick_seat(&config, &state, None, now).ok())
                .or_else(|| state.active_seat.clone())
                .unwrap_or_else(|| config.seats[0].name.clone())
        }
    };
    let entry = config.find(&seat_name).cloned().expect("seat exists");

    if dry_run {
        let before = seat::slot_snapshot(&seat_name);
        let listed = client.list_resets(&entry);
        seat::sync_active_auth(&seat_name, before);
        let listing = listed?;
        if json_out {
            let rows: Vec<serde_json::Value> = listing
                .credits
                .iter()
                .map(|c| json!({ "id": c.id, "title": c.title, "expires_at": c.expires_at }))
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "seat": seat_name,
                    "available": listing.available,
                    "resets": rows,
                }))?
            );
        } else if !listing.credits.is_empty() {
            println!("Free usage-limit resets on seat '{}' (nothing redeemed):", seat_name);
            for c in &listing.credits {
                println!(
                    "  {}  {}{}",
                    c.id,
                    c.title.as_deref().unwrap_or("reset"),
                    c.expires_at
                        .map(|e| format!("  expires {}", e.with_timezone(&Local).format("%a %d %b %H:%M")))
                        .unwrap_or_default()
                );
            }
        } else if listing.available > 0 {
            // The backend sometimes reports only a count, with no detail rows.
            println!(
                "Seat '{}': {} free usage-limit reset(s) available; codex did not list their details, \
                 so redeem without --credit-id.",
                seat_name, listing.available
            );
        } else {
            println!("No free usage-limit resets available on seat '{}'.", seat_name);
        }
        // Having grants is success, whether or not their details were listed.
        return Ok(if listing.available > 0 || !listing.credits.is_empty() { 0 } else { 1 });
    }

    // If this is the active seat, make sure its slot holds the freshest token
    // before it is staged into a scratch home (plain `codex` may have
    // refreshed the global file since the last run), exactly as status does.
    if state.active_seat.as_deref() == Some(seat_name.as_str()) {
        match seat::refresh_back_guarded(&seat_name, &config.identity_for(&seat_name)) {
            Ok(outcome) => seat::warn_refresh_back(&seat_name, &outcome),
            Err(e) => eprintln!("Warning: refresh-back for active seat '{}' failed: {:#}", seat_name, e),
        }
    }
    let before = seat::slot_snapshot(&seat_name);
    let outcome = client.consume_reset(&entry, credit_id);
    seat::sync_active_auth(&seat_name, before);
    let outcome = outcome?;
    log_event("reset", &seat_name, &format!("outcome={} (manual)", outcome.as_str()));

    let mut cleared = false;
    if outcome.is_reset() {
        state.entry_mut(&seat_name).usage_checked_at = Some(Utc::now());
        let before = seat::slot_snapshot(&seat_name);
        let refreshed = client.fetch(&entry);
        seat::sync_active_auth(&seat_name, before);
        match refreshed {
            Ok(snap) => {
                for (s, notice) in
                    usage::reconcile_snapshots(&config, &mut state, vec![(seat_name.clone(), snap)], Utc::now())
                {
                    log_event("status", &s, &notice);
                }
            }
            Err(e) => {
                eprintln!("Warning: could not re-read usage after the reset: {}", e);
                // The grant is spent; do not leave the seat blocked on a
                // reading we now know is stale.
                cleared = usage::invalidate_after_reset(&mut state, &seat_name, Utc::now());
            }
        }
        if !cleared {
            cleared = usage::clear_window_cooldown_after_reset(&mut state, &seat_name, Utc::now());
        }
        state.save()?;
    }

    let st = state.get(&seat_name);
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "seat": seat_name,
                "outcome": outcome.as_str(),
                "cooldown_cleared": cleared,
                "usage": st.usage,
            }))?
        );
    } else {
        match outcome {
            ResetOutcome::Reset => {
                println!("Seat '{}': usage limits reset (one free reset used).", seat_name);
                if let Some(u) = st.usage.as_ref() {
                    println!("  now {}", usage::summarize_usage_short(u));
                }
                if cleared {
                    println!("  its cooldown was cleared; the seat is available again.");
                }
            }
            ResetOutcome::NothingToReset => println!(
                "Seat '{}': nothing to reset — no usage window is currently exhausted. No reset was used.",
                seat_name
            ),
            ResetOutcome::NoCredit => println!(
                "Seat '{}': no free usage-limit resets are available on this account.",
                seat_name
            ),
            ResetOutcome::AlreadyRedeemed => {
                println!("Seat '{}': that reset was already redeemed.", seat_name)
            }
        }
    }
    Ok(if outcome.is_reset() { 0 } else { 1 })
}

// ---------------------------------------------------------------------------
// cost
// ---------------------------------------------------------------------------

/// `codex-clean cost [SESSION_ID | --last] [--seat NAME] [--json]`.
pub fn cost(session: Option<&str>, last: bool, seat_name: Option<&str>, json_out: bool) -> Result<i32> {
    cost_with(&AppServerClient::default(), session, last, seat_name, json_out)
}

/// Injectable core of `cost`.
pub fn cost_with(
    client: &dyn UsageClient,
    session: Option<&str>,
    last: bool,
    seat_name: Option<&str>,
    json_out: bool,
) -> Result<i32> {
    let Some(_lock) = CodexLock::try_acquire()? else {
        bail!(
            "a codex-clean run is in progress (holding {}); retry when it finishes",
            seat::lock_path()?.display()
        );
    };
    let config = SeatConfig::load()?
        .filter(|c| !c.seats.is_empty())
        .ok_or_else(|| anyhow!("no seats configured; run `codex-clean seat add <name>` first"))?;
    let state = SeatState::load()?;

    // Resolve the session and the seat that ran it.
    let (seat_name, session_id) = match (session, last) {
        (Some(id), _) => {
            let owner = seat_name
                .map(String::from)
                .or_else(|| {
                    config
                        .seats
                        .iter()
                        .find(|s| state.get(&s.name).last_session.as_deref() == Some(id))
                        .map(|s| s.name.clone())
                })
                .or_else(|| state.active_seat.clone())
                .ok_or_else(|| anyhow!("could not tell which seat ran that session; pass --seat"))?;
            (owner, id.to_string())
        }
        (None, true) => {
            let mut best: Option<(String, String, chrono::DateTime<Utc>)> = None;
            for s in &config.seats {
                let st = state.get(&s.name);
                // Ordered by when the session was recorded, not by last_used:
                // a later failed run on another seat must not win.
                if let (Some(id), Some(when)) = (st.last_session.clone(), st.last_session_at) {
                    if seat_name.is_none_or(|n| n == s.name)
                        && best.as_ref().is_none_or(|(_, _, w)| when > *w)
                    {
                        best = Some((s.name.clone(), id, when));
                    }
                }
            }
            let (n, id, _) = best.ok_or_else(|| {
                anyhow!("no session recorded yet; run codex-clean once, or pass a session id")
            })?;
            (n, id)
        }
        (None, false) => bail!("pass a session id or --last"),
    };

    let entry = config.find(&seat_name).cloned().ok_or_else(|| anyhow!("seat '{}' not found", seat_name))?;
    // Same as status and reset: if this is the active seat, bring its slot up
    // to date first, so the scratch home is not staged from a stale token.
    if state.active_seat.as_deref() == Some(seat_name.as_str()) {
        match seat::refresh_back_guarded(&seat_name, &config.identity_for(&seat_name)) {
            Ok(outcome) => seat::warn_refresh_back(&seat_name, &outcome),
            Err(e) => eprintln!("Warning: refresh-back for active seat '{}' failed: {:#}", seat_name, e),
        }
    }
    let before = seat::slot_snapshot(&seat_name);
    let got = client.thread_cost(&entry, &session_id);
    seat::sync_active_auth(&seat_name, before);
    let cost = got?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "seat": seat_name,
                "session": session_id,
                "credits_micros": cost.credits_micros,
                "usd_micros": cost.usd_micros,
                "display": usage::format_cost(&cost),
            }))?
        );
    } else {
        println!("Session {} on seat '{}': {}", session_id, seat_name, usage::format_cost(&cost));
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// events
// ---------------------------------------------------------------------------

/// Print the last `tail` lines of `seat-events.log`.
pub fn events(tail: usize) -> Result<()> {
    let path = seat::seat_events_log_path()?;
    let raw = match fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            eprintln!("No seat events recorded yet ({} does not exist).", path.display());
            return Ok(());
        }
        Err(e) => return Err(anyhow::Error::from(e).context(format!("reading {}", path.display()))),
    };
    let lines: Vec<&str> = raw.lines().collect();
    let start = lines.len().saturating_sub(tail);
    for l in &lines[start..] {
        println!("{}", l);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// remove
// ---------------------------------------------------------------------------

pub fn remove(name: &str, yes: bool) -> Result<()> {
    let mut config = SeatConfig::load()?
        .ok_or_else(|| anyhow!("no seats configured"))?;
    if config.find(name).is_none() {
        bail!("seat '{}' not found", name);
    }

    if !yes {
        eprint!(
            "Remove seat '{}' and delete its private auth.json? [y/N] ",
            name
        );
        io::stderr().flush().ok();
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        let a = answer.trim();
        if a != "y" && a != "Y" && a != "yes" {
            eprintln!("Aborted; seat '{}' not removed.", name);
            return Ok(());
        }
    }

    // Lock acquired AFTER the confirmation prompt so we don't block other
    // codex runs while waiting on the user. Once confirmed, hold it for the
    // duration of the file/state mutations to prevent racing with a running
    // codex invocation that might be mid refresh-back to this seat.
    let _lock = CodexLock::acquire()?;

    config.seats.retain(|s| s.name != name);
    if config.rotation.strategy == seat::Strategy::Fixed
        && config.rotation.fixed_seat.as_deref() == Some(name)
    {
        // Otherwise seats.toml would fail validation on every load and the
        // CLI could not repair itself.
        config.rotation.strategy = seat::Strategy::LeastRecentlyUsed;
        config.rotation.fixed_seat = None;
        eprintln!(
            "Seat '{}' was the fixed seat; rotation strategy reset to least-recently-used.",
            name
        );
        log_event("strategy", "-", "reset to least-recently-used (fixed seat removed)");
    }
    config.save()?;

    let mut state = SeatState::load()?;
    state.seats.remove(name);
    if state.active_seat.as_deref() == Some(name) {
        state.active_seat = None;
    }
    state.save()?;

    let dir = seats_dir()?.join(name);
    if dir.exists() {
        fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    eprintln!("Seat '{}' removed.", name);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_seat_name_accepts_simple() {
        assert!(validate_seat_name("personal").is_ok());
        assert!(validate_seat_name("work-pro").is_ok());
        assert!(validate_seat_name("a_b_c").is_ok());
        assert!(validate_seat_name("seat1").is_ok());
    }

    #[test]
    fn validate_seat_name_rejects_invalid() {
        assert!(validate_seat_name("").is_err());
        assert!(validate_seat_name(".").is_err());
        assert!(validate_seat_name("..").is_err());
        assert!(validate_seat_name("with space").is_err());
        assert!(validate_seat_name("with/slash").is_err());
        assert!(validate_seat_name("with.dot").is_err());
    }
}
