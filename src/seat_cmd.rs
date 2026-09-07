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
use crate::usage::{self, AppServerClient, UsageClient, UsageFetchError};

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
        "{:<14} {:<22} {:<17} {:<18} {:<10} {:<22}",
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
                usage::summarize_usage_short(u),
                format!("{} ago", usage::format_duration_short(now - u.fetched_at)),
            ),
            None => ("-".to_string(), "-".to_string()),
        };
        let status = format_status(&st, active.map(|a| a == seat.name).unwrap_or(false), now);
        println!(
            "{:<14} {:<22} {:<17} {:<18} {:<10} {:<22}",
            seat.name,
            truncate(label, 22),
            last_used,
            truncate(&usage_col, 18),
            fetched_col,
            status
        );
    }
    Ok(())
}

fn format_status(st: &SeatRuntimeState, is_active: bool, now: DateTime<Utc>) -> String {
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
pub fn status(name: Option<&str>, json_out: bool, clear_cooldown: Option<&str>) -> Result<i32> {
    status_with(&AppServerClient::default(), name, json_out, clear_cooldown)
}

/// Outcome for one seat, assembled for both the table and `--json`.
struct SeatStatusRow {
    name: String,
    label: Option<String>,
    active: bool,
    state: SeatRuntimeState,
    result: Result<UsageSnapshot, UsageFetchError>,
    notices: Vec<String>,
}

/// Injectable core of `status`. Returns the process exit code.
pub fn status_with(
    client: &dyn UsageClient,
    only: Option<&str>,
    json_out: bool,
    clear_cooldown: Option<&str>,
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
    let mut rows = Vec::with_capacity(results.len());
    for (seat_entry, (_, result)) in targets.iter().zip(results) {
        let mut notices = Vec::new();
        if let Ok(snap) = &result {
            let verdict = usage::verdict(snap);
            let entry = state.entry_mut(&seat_entry.name);
            let mut new_notices = usage::apply_snapshot(
                &seat_entry.name,
                entry,
                snap.clone(),
                &config.rotation,
                now,
            );
            // Workspace-wide reasons cool the siblings too (extend-only), the
            // same way the runner does — otherwise `seat status main` could
            // learn the workspace is out of credits and leave backup1 eligible.
            if let usage::UsageVerdict::Exhausted { reason, .. } = verdict {
                if !reason.is_window_based() {
                    if let Some(until) = state.get(&seat_entry.name).cooldown_until {
                        let siblings: Vec<String> = seat::workspace_siblings(&config, &seat_entry.name)
                            .into_iter()
                            .filter(|n| *n != seat_entry.name)
                            .collect();
                        let changed = seat::cool_seats(&mut state, &siblings, until, reason.as_str(), now);
                        if !changed.is_empty() {
                            new_notices.push(format!(
                                "{} is workspace-wide; also cooling {} until {}",
                                reason,
                                changed.join(", "),
                                until.with_timezone(&Local).format("%a %H:%M")
                            ));
                        }
                    }
                }
            }
            for n in &new_notices {
                log_event("status", &seat_entry.name, n);
            }
            notices.extend(new_notices);
        } else if let Err(e) = &result {
            log_event("status_error", &seat_entry.name, &e.to_string());
        }
        rows.push(SeatStatusRow {
            name: seat_entry.name.clone(),
            label: seat_entry.label.clone(),
            active: active.as_deref() == Some(seat_entry.name.as_str()),
            state: state.get(&seat_entry.name),
            result,
            notices,
        });
    }
    state.save()?;

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
        print_status_json(&rows, &global_notices)?;
    } else {
        print_status_table(&rows, &global_notices, now);
    }
    Ok(if any_ok { 0 } else { 1 })
}

fn print_status_table(rows: &[SeatStatusRow], global_notices: &[String], now: DateTime<Utc>) {
    const W: (usize, usize, usize, usize, usize) = (14, 18, 8, 28, 28);
    println!(
        "{:<w0$} {:<w1$} {:<w2$} {:<w3$} {:<w4$} STATUS",
        "NAME",
        "LABEL",
        "PLAN",
        "5H",
        "WEEKLY",
        w0 = W.0,
        w1 = W.1,
        w2 = W.2,
        w3 = W.3,
        w4 = W.4
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
        let status = format_status(&row.state, row.active, now);
        println!(
            "{:<w0$} {:<w1$} {:<w2$} {:<w3$} {:<w4$} {}",
            row.name,
            truncate(label, W.1),
            truncate(&plan, W.2),
            five_h,
            weekly,
            status,
            w0 = W.0,
            w1 = W.1,
            w2 = W.2,
            w3 = W.3,
            w4 = W.4
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

fn print_status_json(rows: &[SeatStatusRow], global_notices: &[String]) -> Result<()> {
    let seats: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let (usage_v, error_v) = match &r.result {
                Ok(snap) => (serde_json::to_value(snap).unwrap_or(json!(null)), json!(null)),
                Err(e) => (json!(null), json!(e.to_string())),
            };
            json!({
                "name": r.name,
                "label": r.label,
                "active": r.active,
                "needs_login": r.state.needs_login,
                "cooldown_until": r.state.cooldown_until,
                "cooldown_reason": r.state.cooldown_reason,
                "usage": usage_v,
                "error": error_v,
                "notices": r.notices,
            })
        })
        .collect();
    let doc = json!({ "seats": seats, "notices": global_notices });
    println!("{}", serde_json::to_string_pretty(&doc)?);
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
    log_event("strategy", config.rotation.fixed_seat.as_deref().unwrap_or("-"), &format!("set to {}", strategy));
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
