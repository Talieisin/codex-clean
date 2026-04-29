//! Implementations of the `codex-clean seat ...` subcommands.
//!
//! Pure orchestration over `seat.rs`'s data layer. Each function is `pub`
//! and returns `anyhow::Result<()>`; failures bubble up to `main.rs` which
//! prints them and exits non-zero.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Local, Utc};

use crate::seat::{
    self, codex_auth_path, ensure_file_credential_store, read_account_id, refresh_back,
    seat_auth_path, seats_dir, swap_active_auth, CodexLock, FileStoreOutcome, SeatConfig,
    SeatEntry, SeatRuntimeState, SeatState,
};

/// Guard that removes a partial-login directory on drop unless `commit()`
/// has been called. Ensures we never leave a Ctrl-C'd login flow's tokens
/// sitting around in `~/.config/codex-clean/seats/<name>.partial-<pid>/`.
struct PartialLoginDir {
    path: PathBuf,
    committed: bool,
}

impl PartialLoginDir {
    fn create_for(name: &str) -> Result<Self> {
        let path = seats_dir()?
            .join(format!("{}.partial-{}", name, std::process::id()));
        // If a previous run died and left this dir, blow it away.
        let _ = fs::remove_dir_all(&path);
        seat::secure_create_dir_all(&path)?;
        Ok(Self { path, committed: false })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn commit(mut self) {
        self.committed = true;
        // Tempdir is no longer needed; remove eagerly so we don't leave
        // tokens on disc a second longer than necessary.
        let _ = fs::remove_dir_all(&self.path);
    }
}

impl Drop for PartialLoginDir {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

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
    // Seed config.toml in the partial home so codex login writes to a file
    // (rather than the OS keyring). This is critical: if cli_auth_credentials_store
    // resolves to "keyring", auth.json never appears in our temp home.
    let cfg_path = home.join("config.toml");
    fs::write(&cfg_path, "cli_auth_credentials_store = \"file\"\n")
        .with_context(|| format!("writing {}", cfg_path.display()))?;

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
    if config.find(name).is_some() {
        bail!("seat '{}' already exists; remove it first or pick a different name", name);
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
    let active_auth = codex_auth_path()?;
    if !active_auth.exists() {
        bail!(
            "cannot --import: {} does not exist (run `codex login` first, or omit --import)",
            active_auth.display()
        );
    }
    let bytes = fs::read(&active_auth)
        .with_context(|| format!("reading {}", active_auth.display()))?;
    // Propagate read/parse failures (rather than silently importing without
    // an account_id and weakening mismatch protection later); a missing
    // tokens.account_id field is fine and surfaces as Ok(None).
    let account_id = read_account_id(&active_auth)
        .with_context(|| format!("reading account_id from {}", active_auth.display()))?;
    let dest = seat_auth_path(name)?;
    if let Some(parent) = dest.parent() {
        seat::secure_create_dir_all(parent)?;
    }
    seat::atomic_write(&dest, &bytes)?;

    config.seats.push(SeatEntry {
        name: name.to_string(),
        label: label.map(String::from),
        account_id,
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

    eprintln!(
        "Starting login for seat '{}'. The codex CLI will print a URL and code below — open the URL in any browser, sign in to the {}ChatGPT account for this seat, and enter the code.",
        name,
        if is_first_seat { "" } else { "second " }
    );

    // Run codex login against an isolated temp CODEX_HOME so the active
    // ~/.codex/auth.json is never replaced. Ctrl-C in the middle just leaves
    // the partial dir, which the guard cleans up on drop.
    let partial = PartialLoginDir::create_for(name)?;
    spawn_codex_login_in(partial.path(), browser)?;

    let temp_auth = partial.path().join("auth.json");
    let auth_bytes = fs::read(&temp_auth)
        .with_context(|| format!("reading {}", temp_auth.display()))?;
    let account_id = read_account_id(&temp_auth)?;
    if account_id.is_none() {
        eprintln!(
            "Warning: could not extract account_id from the new auth.json; \
             account-mismatch protection on `seat login` will be unavailable for this seat."
        );
    }

    let dest = seat_auth_path(name)?;
    if let Some(parent) = dest.parent() {
        seat::secure_create_dir_all(parent)?;
    }
    seat::atomic_write(&dest, &auth_bytes)?;
    partial.commit();

    config.seats.push(SeatEntry {
        name: name.to_string(),
        label: label.map(String::from),
        account_id,
    });
    config.save()?;

    if is_first_seat {
        let mut state = SeatState::load()?;
        state.active_seat = Some(name.to_string());
        state.save()?;
    }

    if is_first_seat {
        eprintln!("Seat '{}' added.", name);
    } else {
        eprintln!(
            "Seat '{}' added. Run `codex-clean seat use {}` to make it active.",
            name, name
        );
    }
    Ok(())
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
        "{:<14} {:<22} {:<22} {:<22}",
        "NAME", "LABEL", "LAST USED", "STATUS"
    );
    for seat in &config.seats {
        let st = state.get(&seat.name);
        let label = seat.label.as_deref().unwrap_or("-");
        let last_used = match st.last_used {
            Some(t) => format_local(t),
            None => "never".to_string(),
        };
        let status = format_status(&st, active.map(|a| a == seat.name).unwrap_or(false), now);
        println!(
            "{:<14} {:<22} {:<22} {:<22}",
            seat.name, truncate(label, 22), last_used, status
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
            return format!("cooling until {}", until.with_timezone(&Local).format("%-I:%M %p"));
        }
    }
    if is_active {
        "ready (active)".to_string()
    } else {
        "ready".to_string()
    }
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
    let expected_account_id = config
        .find(name)
        .ok_or_else(|| {
            anyhow!(
                "seat '{}' not found; run `codex-clean seat list` to see configured seats",
                name
            )
        })?
        .account_id
        .clone();

    let _lock = CodexLock::acquire()?;
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
    let partial = PartialLoginDir::create_for(name)?;
    spawn_codex_login_in(partial.path(), browser)?;

    let temp_auth = partial.path().join("auth.json");
    let new_account_id = read_account_id(&temp_auth)?;

    // Account-id verification: if the seat already has an account_id stored,
    // refuse to overwrite with a different account.
    if let Some(expected) = expected_account_id.as_ref() {
        match new_account_id.as_ref() {
            Some(got) if got == expected => {}
            Some(got) => {
                bail!(
                    "Account mismatch: seat '{}' was registered for account '{}' but you signed in as '{}'. \
                     The existing tokens were left untouched. \
                     If you genuinely want to repoint this seat, remove and re-add it: \
                     `codex-clean seat remove {} && codex-clean seat add {}`.",
                    name,
                    expected,
                    got,
                    name,
                    name
                );
            }
            None => {
                bail!(
                    "The new auth.json is missing tokens.account_id, so we can't verify it matches \
                     the previously registered account ('{}'). Refusing to overwrite seat '{}'.",
                    expected,
                    name
                );
            }
        }
    }

    let new_auth = fs::read(&temp_auth)
        .with_context(|| format!("reading {}", temp_auth.display()))?;
    let dest = seat_auth_path(name)?;
    if let Some(parent) = dest.parent() {
        seat::secure_create_dir_all(parent)?;
    }
    seat::atomic_write(&dest, &new_auth)?;
    partial.commit();

    // Adopt the new account_id if the seat didn't have one stored yet
    // (e.g. it was added before this field existed).
    if expected_account_id.is_none() {
        if let Some(seat_entry) = config.seats.iter_mut().find(|s| s.name == name) {
            seat_entry.account_id = new_account_id;
        }
    }

    let mut state = SeatState::load()?;
    let entry = state.entry_mut(name);
    entry.needs_login = false;
    entry.consecutive_failures = 0;
    state.save()?;

    // Persist any account_id we may have just adopted. Failing here would
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
        if prev != name {
            refresh_back(prev).with_context(|| {
                format!("refresh-back for previously active seat '{}'", prev)
            })?;
        }
    }

    swap_active_auth(name)?;
    state.active_seat = Some(name.to_string());
    state.save()?;
    eprintln!("Active seat is now '{}'.", name);
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

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn validate_seat_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("seat name cannot be empty");
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!(
            "seat name '{}' contains invalid characters (use [a-zA-Z0-9_-])",
            name
        );
    }
    if name == "." || name == ".." {
        bail!("seat name '{}' is reserved", name);
    }
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
