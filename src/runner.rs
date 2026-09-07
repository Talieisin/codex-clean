use std::env;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;

use anyhow::{Context, Result};
use chrono::{Local, Utc};

use crate::events::{extract_event, Event};
use crate::output::CodexOutput;
use crate::ratelimit::{self, FailureKind};
use crate::seat::{
    self, cool_seats, log_event, log_excerpt, refresh_back_guarded, seat_notice, swap_active_auth,
    unmatched_log_path, warn_refresh_back, workspace_siblings, CodexLock, SeatConfig,
    SeatPickError, SeatState, Strategy, SCRUB_ENV_VARS,
};
use crate::usage::{self, AppServerClient, UsageClient};

const STDERR_CAP_BYTES: usize = 10 * 1024 * 1024;

/// `EX_TEMPFAIL`: every seat is cooling; try again later.
pub const EXIT_ALL_SEATS_COOLING: i32 = 75;

/// Target for resume command
pub enum ResumeTarget {
    /// Resume a specific session by ID
    SessionId(String),
    /// Resume the most recent session
    Last,
}

/// Execution mode for codex
pub enum Mode {
    /// Run a new exec session
    Exec,
    /// Resume an existing session
    Resume(ResumeTarget),
    /// Run a code review
    Review,
}

/// Result of a single codex invocation, captured but not yet printed.
pub struct AttemptResult {
    pub output: CodexOutput,
    pub stderr_buffer: Vec<u8>,
    pub stderr_truncated: bool,
    pub stderr_error: Option<io::Error>,
    /// Codex's exit code, escalated to 1 if codex exited 0 but emitted error events.
    pub exit_code: i32,
    pub status_success: bool,
    pub child_exit: i32,
}

/// Run codex with the given arguments and prompt. Drives the multi-seat
/// orchestration if seats are configured; otherwise behaves identically to
/// the pre-seat version.
pub fn run_codex(args: &[String], prompt: &str, mode: Mode) -> Result<i32> {
    run_codex_with(args, prompt, mode, attempt_codex)
}

/// `run_codex_with_client` with the production app-server usage client.
pub fn run_codex_with<F>(args: &[String], prompt: &str, mode: Mode, attempt: F) -> Result<i32>
where
    F: Fn(&[String], &str, &Mode, bool) -> Result<AttemptResult>,
{
    run_codex_with_client(args, prompt, mode, attempt, &AppServerClient::default())
}

/// Internal orchestration that drives the lock/swap/spawn/classify state
/// machine. Generic over the codex attempt callback so tests can inject a
/// fake spawner without touching real auth.json or running real codex, and
/// over the usage client the `balanced` strategy uses to refresh snapshots.
pub fn run_codex_with_client<F>(
    args: &[String],
    prompt: &str,
    mode: Mode,
    attempt: F,
    usage_client: &dyn UsageClient,
) -> Result<i32>
where
    F: Fn(&[String], &str, &Mode, bool) -> Result<AttemptResult>,
{
    let cfg_opt = SeatConfig::load().context("loading seats.toml")?;
    let cfg = match cfg_opt {
        Some(c) if !c.seats.is_empty() => c,
        _ => {
            // Backwards-compat: no seats configured → run as today.
            let result = attempt(args, prompt, &mode, false)?;
            print_attempt(&result);
            return Ok(result.exit_code);
        }
    };

    // Multi-seat path. Lock held for the entire orchestration window —
    // concurrent codex-clean invocations serialise. This matches multi-auth's
    // single-codex constraint and avoids the auth.json refresh-write race.
    let _lock = CodexLock::acquire().context("acquiring codex.lock")?;

    // Re-validate config.toml on every multi-seat run. If somebody flipped
    // cli_auth_credentials_store back to "keyring", subsequent codex spawns
    // would silently use the OS keyring instead of our swapped auth.json.
    let store_outcome = seat::ensure_file_credential_store()
        .context("validating ~/.codex/config.toml credential store setting")?;
    if !matches!(store_outcome, seat::FileStoreOutcome::AlreadyFile) {
        eprintln!(
            "Note: re-applied cli_auth_credentials_store = \"file\" to ~/.codex/config.toml."
        );
    }

    let override_seat = env::var("CODEX_CLEAN_SEAT").ok().filter(|s| !s.is_empty());
    let mut state = SeatState::load()?;
    let max_attempts = cfg.rotation.max_retries.saturating_add(1);
    let mut last_failure: Option<AttemptResult> = None;
    let mut tried_seats: Vec<String> = Vec::new();

    for attempt_index in 0..max_attempts {
        // Before anything touches the slots, stash any token refresh the
        // previously active seat received (from plain `codex`, or from our
        // own last attempt). This must precede a balanced-strategy snapshot
        // refresh too: that stages the slot blob into a scratch home, and a
        // stale refresh token there would be rejected as reused. Runs even
        // when the same seat is about to be re-picked, so a single-seat /
        // pinned run never clobbers a fresher global blob with the stale
        // slot copy. Guarded by identity; never fatal.
        if let Some(prev) = state.active_seat.clone() {
            persist_refresh(&cfg, &prev);
        }

        if attempt_index == 0 && cfg.rotation.strategy == Strategy::Balanced && override_seat.is_none() {
            refresh_stale_usage(&cfg, &mut state, usage_client);
        }

        let now = Utc::now();
        let chosen = match seat::pick_seat_excluding(&cfg, &state, override_seat.as_deref(), now, &tried_seats) {
            Ok(name) => name,
            Err(blocked @ SeatPickError::AllSeatsBlocked { .. }) => {
                let code = report_all_blocked(&blocked);
                if let Some(prev) = last_failure {
                    print_attempt(&prev);
                }
                print_seat_notice(&cfg, &state);
                return Ok(code);
            }
            Err(e) => {
                if let Some(prev) = last_failure {
                    eprintln!("{}", e);
                    print_attempt(&prev);
                    print_seat_notice(&cfg, &state);
                    return Ok(prev.exit_code);
                }
                // A pinned seat that is cooling / needs login: still tell a
                // stdout-only caller why nothing ran.
                print_seat_notice(&cfg, &state);
                anyhow::bail!("{}", e);
            }
        };

        if tried_seats.contains(&chosen) {
            // We've already tried this seat in this run — guard against loops.
            break;
        }
        tried_seats.push(chosen.clone());

        // Eager state update: write last_used before spawning so a future
        // pick (after retry) doesn't reselect the same seat by accident.
        state.entry_mut(&chosen).last_used = Some(now);
        state.save()?;

        swap_active_auth(&chosen)
            .with_context(|| format!("swapping active auth to seat '{}'", chosen))?;
        state.active_seat = Some(chosen.clone());
        state.save()?;

        let attempt = attempt(args, prompt, &mode, true)?;
        // Codex may have refreshed the OAuth token during the run; persist
        // it into the side store so the next swap doesn't install stale
        // credentials.
        persist_refresh(&cfg, &chosen);

        let kind = classify_attempt(&attempt);
        match kind {
            FailureKind::Other if attempt.exit_code == 0 && attempt.output.errors.is_empty() => {
                let entry = state.entry_mut(&chosen);
                entry.consecutive_failures = 0;
                entry.cooldown_until = None;
                entry.cooldown_reason = None;
                state.save()?;
                print_attempt(&attempt);
                print_seat_notice(&cfg, &state);
                return Ok(attempt.exit_code);
            }
            FailureKind::AuthError => {
                let entry = state.entry_mut(&chosen);
                entry.needs_login = true;
                state.save()?;
                log_event(
                    "auth_error",
                    &chosen,
                    &format!("marked needs_login; {}", failure_excerpt(&attempt)),
                );
                eprintln!(
                    "Seat '{}' has invalid credentials. Run: codex-clean seat login {}",
                    chosen, chosen
                );
                print_attempt(&attempt);
                print_seat_notice(&cfg, &state);
                return Ok(attempt.exit_code);
            }
            FailureKind::RateLimit { recovery, reason } => {
                let cd = ratelimit::apply_recovery_window(
                    recovery,
                    Utc::now(),
                    ratelimit::default_cooldown_for(
                        reason,
                        cfg.rotation.default_cooldown_seconds,
                        cfg.rotation.cooldown_max_seconds,
                    ),
                    cfg.rotation.cooldown_min_seconds,
                    cfg.rotation.cooldown_max_seconds,
                    cfg.rotation.cooldown_jitter_seconds,
                );
                // Personal windows are per user: cool this seat only. Credits
                // and spend caps are per *workspace*: every seat sharing this
                // seat's account_id is equally blocked, so cool them together
                // rather than burning an attempt discovering it.
                let affected = affected_seats(&cfg, &chosen, reason);
                // Extend-only: a sibling already cooling for longer keeps its
                // later deadline (and its own reason).
                let _changed = cool_seats(&mut state, &affected, cd, reason.as_str(), Utc::now());
                let entry = state.entry_mut(&chosen);
                entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
                state.save()?;
                let until = cd.with_timezone(&Local).format("%a %H:%M").to_string();
                if affected.len() > 1 {
                    eprintln!(
                        "Seat '{}' exhausted ({}); this applies to the whole workspace, so seats {} are all cooling until {}.",
                        chosen,
                        reason,
                        affected.join(", "),
                        until
                    );
                } else {
                    eprintln!("Seat '{}' exhausted ({}); cooling until {}.", chosen, reason, until);
                }
                log_event(
                    "rate_limit",
                    &chosen,
                    &format!(
                        "reason={} until={} affected={} {}",
                        reason,
                        cd.to_rfc3339(),
                        affected.join(","),
                        failure_excerpt(&attempt)
                    ),
                );
                last_failure = Some(attempt);
                if override_seat.is_some() {
                    break;
                }
                continue;
            }
            FailureKind::Other => {
                let _ = log_unmatched(&chosen, &attempt);
                print_attempt(&attempt);
                print_seat_notice(&cfg, &state);
                return Ok(attempt.exit_code);
            }
        }
    }

    // Rotation ran out of seats to try. If we are not pinned and every
    // configured seat was either rate-limited in this run or is otherwise
    // ineligible right now, that is EX_TEMPFAIL — the same situation the
    // up-front check reports — regardless of how short the cooldowns are.
    // A pinned seat keeps the child's exit code (README contract).
    match last_failure {
        Some(prev) => {
            let now = Utc::now();
            let all_blocked = override_seat.is_none()
                && cfg.seats.iter().all(|s| {
                    tried_seats.contains(&s.name) || !state.get(&s.name).is_eligible(now)
                });
            if all_blocked {
                let code = report_all_blocked(&seat::all_blocked_error(&cfg, &state));
                print_attempt(&prev);
                print_seat_notice(&cfg, &state);
                return Ok(code);
            }
            print_attempt(&prev);
            print_seat_notice(&cfg, &state);
            Ok(prev.exit_code)
        }
        None => Ok(1),
    }
}

/// For the `balanced` strategy: refresh the usage snapshot of every eligible
/// seat whose snapshot is missing or older than `balance_refresh_seconds`,
/// so the pick reflects real headroom. Costs one app-server round-trip per
/// stale seat, so most runs pay nothing. Never fails the run: a seat whose
/// fetch fails keeps its old snapshot (or none, which counts as unused).
fn refresh_stale_usage(cfg: &SeatConfig, state: &mut SeatState, client: &dyn UsageClient) {
    let now = Utc::now();
    let max_age = chrono::Duration::seconds(cfg.rotation.balance_refresh_seconds as i64);
    let stale: Vec<seat::SeatEntry> = cfg
        .seats
        .iter()
        .filter(|s| {
            let st = state.get(&s.name);
            st.is_eligible(now) && st.usage_is_stale(now, max_age)
        })
        .cloned()
        .collect();
    if stale.is_empty() {
        return;
    }
    let names: Vec<&str> = stale.iter().map(|s| s.name.as_str()).collect();
    eprintln!("Refreshing usage for seat(s) {} (balanced strategy).", names.join(", "));

    // The fetch may rotate a seat's token in its slot. For the active seat,
    // ~/.codex/auth.json must follow, or an early exit (all seats cooling)
    // would leave the old refresh token in place and the next run would copy
    // it back over the fresh slot.
    let active = state.active_seat.clone();
    let active_slot_before = active
        .as_deref()
        .and_then(|a| seat::seat_auth_path(a).ok())
        .and_then(|p| std::fs::read(p).ok());

    for (name, result) in usage::fetch_all(client, &stale) {
        match result {
            Ok(snap) => {
                let verdict = usage::verdict(&snap);
                let notices = usage::apply_snapshot(&name, state.entry_mut(&name), snap, &cfg.rotation, now);
                if let usage::UsageVerdict::Exhausted { reason, .. } = verdict {
                    if !reason.is_window_based() {
                        if let Some(until) = state.get(&name).cooldown_until {
                            let siblings: Vec<String> = workspace_siblings(cfg, &name)
                                .into_iter()
                                .filter(|n| *n != name)
                                .collect();
                            cool_seats(state, &siblings, until, reason.as_str(), now);
                        }
                    }
                }
                for n in notices {
                    eprintln!("Note: {}", n);
                    log_event("status", &name, &n);
                }
            }
            Err(usage::UsageFetchError::AuthRequired) => {
                // The seat's tokens are dead; do not let a zero score send
                // the run straight into an auth failure with no fallback.
                state.entry_mut(&name).needs_login = true;
                log_event("auth_error", &name, "usage refresh rejected the seat's tokens; marked needs_login");
                eprintln!(
                    "Seat '{}' has invalid credentials (found while refreshing usage). Run: codex-clean seat login {}",
                    name, name
                );
            }
            Err(e) => eprintln!("Warning: could not refresh usage for seat '{}': {}", name, e),
        }
    }
    if let Err(e) = state.save() {
        eprintln!("Warning: could not save refreshed usage: {:#}", e);
    }

    if let Some(a) = active.as_deref() {
        let after = seat::seat_auth_path(a).ok().and_then(|p| std::fs::read(p).ok());
        if after.is_some() && after != active_slot_before {
            match swap_active_auth(a) {
                Ok(()) => eprintln!(
                    "Note: seat '{}' refreshed its token during the usage check; ~/.codex/auth.json updated.",
                    a
                ),
                Err(e) => eprintln!(
                    "Warning: could not update ~/.codex/auth.json with seat '{}''s refreshed token: {:#}",
                    a, e
                ),
            }
        }
    }
}

/// Seats to cool for a failure on `chosen`: just it for a personal window
/// limit; every seat in the same workspace for credits / spend caps.
fn affected_seats(cfg: &SeatConfig, chosen: &str, reason: ratelimit::CooldownReason) -> Vec<String> {
    if reason.is_window_based() {
        vec![chosen.to_string()]
    } else {
        workspace_siblings(cfg, chosen)
    }
}

/// The degraded-pool summary goes on **stdout**, after the normal output, on
/// every multi-seat run. Background callers read stdout; stderr is discarded.
fn print_seat_notice(cfg: &SeatConfig, state: &SeatState) {
    if let Some(n) = seat_notice(cfg, state, Utc::now()) {
        println!();
        println!("{}", n);
    }
}

/// Short, single-line description of why an attempt failed, for the events log.
fn failure_excerpt(attempt: &AttemptResult) -> String {
    let text = if !attempt.output.errors.is_empty() {
        attempt.output.errors.join(" | ")
    } else if let Some(m) = attempt.output.messages.last() {
        m.clone()
    } else {
        String::from_utf8_lossy(&attempt.stderr_buffer)
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .to_string()
    };
    let text: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let mut out: String = text.trim().chars().take(300).collect();
    if text.trim().chars().count() > 300 {
        out.push('…');
    }
    format!("exit={} msg={:?}", attempt.exit_code, out)
}

/// Persist the active `~/.codex/auth.json` into `seat`'s slot if it belongs
/// to that seat. Warns (never fails) on skip or error.
fn persist_refresh(cfg: &SeatConfig, seat_name: &str) {
    let expected = cfg.identity_for(seat_name);
    match refresh_back_guarded(seat_name, &expected) {
        Ok(outcome) => warn_refresh_back(seat_name, &outcome),
        Err(e) => eprintln!(
            "Warning: failed to persist refreshed token for seat '{}' to its side store: {:#}. \
             If subsequent runs fail with auth errors, run `codex-clean seat login {}`.",
            seat_name, e, seat_name
        ),
    }
}

/// Print the "nothing eligible" message and return the exit code: 75 when at
/// least one seat is merely cooling (retry later), 1 when every seat needs a
/// login (user action required, so a retry loop would spin for nothing).
fn report_all_blocked(err: &SeatPickError) -> i32 {
    eprintln!("{}.", err);
    match err {
        SeatPickError::AllSeatsBlocked { cooling, .. } if *cooling > 0 => EXIT_ALL_SEATS_COOLING,
        _ => 1,
    }
}

/// Classify an attempt's outcome, falling back to stderr text when the
/// structured `output.errors` list is empty (e.g. codex died before
/// emitting any JSON events).
fn classify_attempt(attempt: &AttemptResult) -> FailureKind {
    if !attempt.output.errors.is_empty() {
        let kind = ratelimit::classify(&attempt.output.errors);
        if !matches!(kind, FailureKind::Other) {
            return kind;
        }
    }
    if !attempt.status_success || attempt.exit_code != 0 {
        // Codex sometimes surfaces exhaustion as the *final agent message*
        // rather than an error event ("Your workspace is out of credits. Add
        // credits to continue." arrives as task_complete). Only the LAST
        // message is consulted, only its opening (provider notices are one
        // short sentence, whereas agent prose that merely mentions credits is
        // long), and only when the run failed.
        let mut blob = String::from_utf8_lossy(&attempt.stderr_buffer).into_owned();
        if let Some(last) = attempt.output.messages.last() {
            blob.push('\n');
            blob.extend(last.chars().take(FINAL_MESSAGE_CLASSIFY_CHARS));
        }
        return ratelimit::classify_text(&blob);
    }
    FailureKind::Other
}

/// How much of a failed run's final agent message is examined for an
/// exhaustion notice. Codex's own notices lead with the sentence.
const FINAL_MESSAGE_CLASSIFY_CHARS: usize = 200;

/// Record an unclassified failure for offline pattern tuning. The log is
/// 0600 (via `append_private_log`): the captured text can include model
/// output and error payloads, so it is treated like a credential file. The
/// parsed error events and the final agent message are included — they are
/// where codex puts the sentence we failed to match, and stderr alone has
/// proven useless for diagnosis.
fn log_unmatched(seat: &str, attempt: &AttemptResult) -> Result<()> {
    let path = unmatched_log_path()?;
    let stderr = String::from_utf8_lossy(&attempt.stderr_buffer);
    let tail: Vec<&str> = stderr.lines().rev().take(20).collect();
    let tail = log_excerpt(&tail.into_iter().rev().collect::<Vec<_>>().join("\n"), 4000);
    let errors = if attempt.output.errors.is_empty() {
        "(none)".to_string()
    } else {
        log_excerpt(&attempt.output.errors.join("\n  "), 2000)
    };
    let last_message = attempt
        .output
        .messages
        .last()
        .map(|m| log_excerpt(m, 500))
        .unwrap_or_else(|| "(none)".to_string());
    let entry = format!(
        "{} seat={} exit={} errors<<<\n  {}\n>>> last_message<<<\n{}\n>>> stderr_tail<<<\n{}\n>>>\n",
        Utc::now().to_rfc3339(),
        seat,
        attempt.exit_code,
        errors,
        last_message,
        tail
    );
    seat::append_private_log(&path, &entry)
}

/// Print captured stderr (when failure) and the formatted output. Mirrors
/// the pre-seat printing behaviour exactly.
pub fn print_attempt(attempt: &AttemptResult) {
    if !attempt.status_success {
        if !attempt.stderr_buffer.is_empty() {
            eprintln!("--- codex stderr ---");
            let _ = io::stderr().write_all(&attempt.stderr_buffer);
            if attempt.stderr_truncated {
                eprintln!("(stderr truncated to {} bytes)", STDERR_CAP_BYTES);
            }
            if let Some(err) = &attempt.stderr_error {
                eprintln!("(failed to capture full stderr: {})", err);
            }
            eprintln!("--- end stderr ---");
        } else if let Some(err) = &attempt.stderr_error {
            eprintln!("--- codex stderr ---");
            eprintln!("Failed to capture stderr: {}", err);
            eprintln!("--- end stderr ---");
        }

        if attempt.output.lines_seen == 0 {
            eprintln!("Codex exited with code {} and produced no JSON output", attempt.child_exit);
        } else if attempt.output.events_recognized == 0 {
            eprintln!(
                "Codex exited with code {} and produced no recognized JSON events",
                attempt.child_exit
            );
        }
    } else if let Some(err) = &attempt.stderr_error {
        eprintln!("Warning: Failed to capture codex stderr: {}", err);
    }

    attempt.output.print();
}

/// One codex spawn-and-collect cycle. Captures stdout/stderr but does not
/// print them; callers decide whether this attempt is the "final" one to
/// surface to the user.
pub fn attempt_codex(
    args: &[String],
    prompt: &str,
    mode: &Mode,
    scrub_env: bool,
) -> Result<AttemptResult> {
    let mut cmd = Command::new("codex");

    // All modes use "codex exec" with --json for JSON output
    cmd.arg("exec");

    if scrub_env {
        for var in SCRUB_ENV_VARS {
            cmd.env_remove(var);
        }
    }

    let mut use_stdin_for_prompt = false;

    match mode {
        Mode::Exec => {
            cmd.arg("--json");
            cmd.arg("--skip-git-repo-check");
            cmd.args(args);
            cmd.arg(prompt);
        }
        Mode::Resume(target) => {
            cmd.arg("--json");
            cmd.arg("--skip-git-repo-check");
            cmd.arg("resume");
            match target {
                ResumeTarget::SessionId(id) => {
                    cmd.arg(id);
                    if !prompt.is_empty() {
                        cmd.arg(prompt);
                    }
                }
                ResumeTarget::Last => {
                    cmd.arg("--last");
                    if !prompt.is_empty() {
                        use_stdin_for_prompt = true;
                    }
                }
            }
        }
        Mode::Review => {
            cmd.arg("review");
            cmd.arg("--json");
            cmd.arg("--skip-git-repo-check");
            cmd.args(args);
            if !prompt.is_empty() {
                cmd.arg(prompt);
            }
        }
    }

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    if use_stdin_for_prompt {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }

    let mut child = cmd.spawn().context("Failed to spawn codex process")?;

    if use_stdin_for_prompt {
        if let Some(mut stdin) = child.stdin.take() {
            writeln!(stdin, "{}", prompt)?;
            stdin.flush()?;
        }
    }

    let stderr = child.stderr.take().expect("stderr was piped");
    let stderr_handle = thread::spawn(move || capture_stderr(stderr));

    let stdout = child.stdout.take().expect("stdout was piped");
    let reader = BufReader::new(stdout);
    let parse_result = parse_codex_stream(reader);

    if parse_result.is_err() {
        let _ = child.kill();
    }

    let status: ExitStatus = child.wait().context("Failed to wait for codex process")?;
    let (stderr_buffer, stderr_truncated, stderr_error) =
        stderr_handle.join().expect("stderr thread panicked");
    let output = parse_result.context("Failed to read codex stdout")?;

    let child_exit = status.code().unwrap_or(1);
    let exit_code = if child_exit == 0 && !output.errors.is_empty() {
        1
    } else {
        child_exit
    };

    Ok(AttemptResult {
        output,
        stderr_buffer,
        stderr_truncated,
        stderr_error,
        exit_code,
        status_success: status.success(),
        child_exit,
    })
}

pub fn parse_codex_stream<R: BufRead>(reader: R) -> io::Result<CodexOutput> {
    let mut output = CodexOutput::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        output.lines_seen += 1;

        if let Some(event) = extract_event(&line) {
            output.events_recognized += 1;
            match event {
                Event::ThreadStarted { thread_id } => {
                    output.add_thread_id(thread_id);
                }
                Event::AgentMessage { text } => {
                    if let Some(t) = text {
                        output.add_message(t);
                    }
                }
                Event::TurnCompleted {
                    input_tokens,
                    cached_input_tokens,
                    output_tokens,
                    reasoning_output_tokens,
                } => {
                    output.add_usage(
                        input_tokens,
                        cached_input_tokens,
                        output_tokens,
                        reasoning_output_tokens,
                    );
                }
                Event::TurnFailed { message } | Event::StreamError { message } => {
                    output.add_error(message);
                }
            }
        }
    }

    Ok(output)
}

fn capture_stderr(stderr: impl Read) -> (Vec<u8>, bool, Option<io::Error>) {
    let mut reader = BufReader::new(stderr);
    let mut buffer = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 4096];

    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let remaining = STDERR_CAP_BYTES.saturating_sub(buffer.len());
                if remaining == 0 {
                    truncated = true;
                    continue;
                }

                let to_copy = remaining.min(n);
                buffer.extend_from_slice(&chunk[..to_copy]);
                if to_copy < n {
                    truncated = true;
                }
            }
            Err(e) => return (buffer, truncated, Some(e)),
        }
    }

    (buffer, truncated, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parse_codex_stream_extracts_events() {
        let data = r#"
{"type":"thread.started","thread_id":"session-1"}
{"type":"item.completed","item":{"type":"agent_message","text":"hello"}}
{"type":"item.completed","item":{"type":"agent_message","text":"world"}}
"#;
        let cursor = Cursor::new(data);
        let output = parse_codex_stream(BufReader::new(cursor)).unwrap();
        assert_eq!(output.session_id, Some("session-1".to_string()));
        assert_eq!(output.messages, vec!["hello".to_string(), "world".to_string()]);
    }

    #[test]
    fn parse_codex_stream_extracts_usage() {
        let data = r#"
{"type":"thread.started","thread_id":"session-1"}
{"type":"item.completed","item":{"type":"agent_message","text":"hello"}}
{"type":"turn.completed","usage":{"input_tokens":15228,"cached_input_tokens":14208,"output_tokens":249,"reasoning_output_tokens":64}}
"#;
        let cursor = Cursor::new(data);
        let output = parse_codex_stream(BufReader::new(cursor)).unwrap();
        assert_eq!(output.session_id, Some("session-1".to_string()));
        assert_eq!(output.messages, vec!["hello".to_string()]);
        assert_eq!(output.usage, Some((15228, 14208, 249, 64)));
    }

    #[test]
    fn parse_codex_stream_captures_turn_failed() {
        let data = r#"
{"type":"thread.started","thread_id":"session-err"}
{"type":"turn.started"}
{"type":"turn.failed","error":{"message":"invalid_request_error: bad effort"}}
"#;
        let cursor = Cursor::new(data);
        let output = parse_codex_stream(BufReader::new(cursor)).unwrap();
        assert_eq!(output.session_id, Some("session-err".to_string()));
        assert_eq!(output.errors.len(), 1);
        assert!(output.errors[0].contains("invalid_request_error"));
    }

    #[test]
    fn parse_codex_stream_captures_stream_error() {
        let data = r#"
{"type":"thread.started","thread_id":"session-err"}
{"type":"error","message":"connection reset"}
"#;
        let cursor = Cursor::new(data);
        let output = parse_codex_stream(BufReader::new(cursor)).unwrap();
        assert_eq!(output.errors, vec!["connection reset".to_string()]);
    }

    #[test]
    fn parse_codex_stream_tracks_line_counts() {
        let data = r#"
{"type":"thread.started","thread_id":"s1"}
{"type":"unknown.thing","data":"ignored"}
{"type":"item.completed","item":{"type":"agent_message","text":"hi"}}
not json at all
"#;
        let cursor = Cursor::new(data);
        let output = parse_codex_stream(BufReader::new(cursor)).unwrap();
        assert_eq!(output.lines_seen, 4);
        assert_eq!(output.events_recognized, 2); // thread.started + agent_message
    }

    #[test]
    fn parse_codex_stream_all_unrecognized() {
        let data = r#"
{"type":"new.unknown","data":"x"}
{"type":"another.unknown","data":"y"}
"#;
        let cursor = Cursor::new(data);
        let output = parse_codex_stream(BufReader::new(cursor)).unwrap();
        assert_eq!(output.lines_seen, 2);
        assert_eq!(output.events_recognized, 0);
        let rendered = output.render();
        assert!(rendered.stderr.contains("none matched known event types"));
    }

    #[test]
    fn parse_codex_stream_propagates_errors() {
        // Invalid UTF-8 sequence should trigger an error from lines()
        let data = b"\x80\x80";
        let cursor = Cursor::new(&data[..]);
        let err = parse_codex_stream(BufReader::new(cursor)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
