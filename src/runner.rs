use std::env;
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;

use anyhow::{Context, Result};
use chrono::{Local, Utc};

use crate::events::{extract_event, Event};
use crate::output::CodexOutput;
use crate::ratelimit::{self, FailureKind};
use crate::seat::{
    self, refresh_back, swap_active_auth, unmatched_log_path, CodexLock, SeatConfig,
    SeatPickError, SeatState,
};

const STDERR_CAP_BYTES: usize = 10 * 1024 * 1024;

/// Env vars we strip from the codex child process so the active seat's
/// auth.json is the only thing in scope. `CODEX_HOME` is *not* on this list:
/// we honour the user's setting and use it as the swap target.
const SCRUB_ENV_VARS: &[&str] = &[
    "CODEX_SQLITE_HOME",
    "OPENAI_API_KEY",
    "CODEX_INTERNAL_ORIGINATOR_OVERRIDE",
    "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
    "CODEX_SANDBOX",
    "CODEX_CLEAN_SEAT",
];

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

/// Internal orchestration that drives the lock/swap/spawn/classify state
/// machine. Generic over the codex attempt callback so tests can inject a
/// fake spawner without touching real auth.json or running real codex.
pub fn run_codex_with<F>(args: &[String], prompt: &str, mode: Mode, attempt: F) -> Result<i32>
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

    for _ in 0..max_attempts {
        let now = Utc::now();
        let chosen = match seat::pick_seat(&cfg, &state, override_seat.as_deref(), now) {
            Ok(name) => name,
            Err(SeatPickError::AllSeatsBlocked { soonest_name, soonest_until }) => {
                let when = soonest_until
                    .map(|u| u.with_timezone(&Local).format("%-I:%M %p").to_string())
                    .unwrap_or_else(|| "later".to_string());
                let who = soonest_name
                    .map(|n| format!(" (seat '{}')", n))
                    .unwrap_or_default();
                eprintln!("All seats cooling; soonest available at {}{}.", when, who);
                if let Some(prev) = last_failure {
                    print_attempt(&prev);
                }
                return Ok(75);
            }
            Err(e) => {
                if let Some(prev) = last_failure {
                    eprintln!("{}", e);
                    print_attempt(&prev);
                    return Ok(prev.exit_code);
                }
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
        if let Err(e) = refresh_back(&chosen) {
            // Codex may have refreshed the OAuth token during the run. If we
            // can't persist that refresh into the side store, the next swap
            // would install stale credentials. Surface it so the user knows
            // why a future run might fail with an auth error.
            eprintln!(
                "Warning: failed to persist refreshed token for seat '{}' to its side store: {:#}. \
                 If subsequent runs fail with auth errors, run `codex-clean seat login {}`.",
                chosen, e, chosen
            );
        }

        let kind = classify_attempt(&attempt);
        match kind {
            FailureKind::Other if attempt.exit_code == 0 && attempt.output.errors.is_empty() => {
                let entry = state.entry_mut(&chosen);
                entry.consecutive_failures = 0;
                entry.cooldown_until = None;
                state.save()?;
                print_attempt(&attempt);
                return Ok(attempt.exit_code);
            }
            FailureKind::AuthError => {
                let entry = state.entry_mut(&chosen);
                entry.needs_login = true;
                state.save()?;
                eprintln!(
                    "Seat '{}' has invalid credentials. Run: codex-clean seat login {}",
                    chosen, chosen
                );
                print_attempt(&attempt);
                return Ok(attempt.exit_code);
            }
            FailureKind::RateLimit { recovery } => {
                let cd = ratelimit::apply_recovery_window(
                    recovery,
                    Utc::now(),
                    cfg.rotation.default_cooldown_seconds,
                    cfg.rotation.cooldown_min_seconds,
                    cfg.rotation.cooldown_max_seconds,
                    cfg.rotation.cooldown_jitter_seconds,
                );
                let entry = state.entry_mut(&chosen);
                entry.cooldown_until = Some(cd);
                entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
                state.save()?;
                eprintln!(
                    "Seat '{}' rate-limited; cooling until {}.",
                    chosen,
                    cd.with_timezone(&Local).format("%-I:%M %p")
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
                return Ok(attempt.exit_code);
            }
        }
    }

    if let Some(prev) = last_failure {
        print_attempt(&prev);
        Ok(prev.exit_code)
    } else {
        Ok(1)
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
        let stderr = String::from_utf8_lossy(&attempt.stderr_buffer);
        return ratelimit::classify_text(&stderr);
    }
    FailureKind::Other
}

fn log_unmatched(seat: &str, attempt: &AttemptResult) -> Result<()> {
    let path = unmatched_log_path()?;
    if let Some(p) = path.parent() {
        // 0700 on the parent dir; 0600 on the file itself. The captured
        // stderr tail can include sensitive context (model output, partial
        // tokens, error payloads) so we treat this log like a credential
        // file rather than a default-perms application log.
        seat::secure_create_dir_all(p)?;
    }
    let stderr = String::from_utf8_lossy(&attempt.stderr_buffer);
    let tail: Vec<&str> = stderr.lines().rev().take(20).collect();
    let tail = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
    let entry = format!(
        "{} seat={} exit={} stderr_tail<<<\n{}\n>>>\n",
        Utc::now().to_rfc3339(),
        seat,
        attempt.exit_code,
        tail
    );
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;

    // Tighten perms via the open file descriptor (fchmod) before writing.
    // Path-based set_permissions would race with another process replacing
    // the path between our open and the chmod; using the fd we can't be
    // pointed at a different file. Mode is only applied on creation, so for
    // a pre-existing log file (created by an older build at the umask
    // default) this is the only path that tightens it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = f.metadata() {
            let mut perms = meta.permissions();
            if perms.mode() & 0o777 != 0o600 {
                perms.set_mode(0o600);
                let _ = f.set_permissions(perms);
            }
        }
    }

    f.write_all(entry.as_bytes())
        .with_context(|| format!("writing to {}", path.display()))?;
    Ok(())
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
