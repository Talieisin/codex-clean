use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;

use anyhow::{Context, Result};

use crate::events::{extract_event, Event};
use crate::output::CodexOutput;

const STDERR_CAP_BYTES: usize = 10 * 1024 * 1024;

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

/// Run codex with the given arguments and prompt
pub fn run_codex(args: &[String], prompt: &str, mode: Mode) -> Result<i32> {
    let mut cmd = Command::new("codex");

    // All modes use "codex exec" with --json for JSON output
    cmd.arg("exec");

    // Track if we need to send prompt via stdin (required for --last)
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
                    // With --last, prompt must come via stdin (codex CLI limitation)
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
    // Codex >=0.123 reads additional input from stdin until EOF unless
    // stdin is piped+closed or /dev/null. If we inherit a long-lived stdin
    // from the parent (e.g. Claude Code), codex hangs indefinitely waiting
    // for EOF. Force stdin closed except when we need to send the prompt.
    if use_stdin_for_prompt {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }

    let mut child = cmd.spawn().context("Failed to spawn codex process")?;

    // Write prompt to stdin if needed (for --last mode)
    if use_stdin_for_prompt {
        if let Some(mut stdin) = child.stdin.take() {
            writeln!(stdin, "{}", prompt)?;
            stdin.flush()?;
            // stdin is dropped here, closing it
        }
    }

    // Capture stderr in a separate thread
    let stderr = child.stderr.take().expect("stderr was piped");
    let stderr_handle = thread::spawn(move || capture_stderr(stderr));

    // Process stdout line by line
    let stdout = child.stdout.take().expect("stdout was piped");
    let reader = BufReader::new(stdout);
    let parse_result = parse_codex_stream(reader);

    // If parsing failed early, kill the child to avoid a deadlock:
    // the child may block writing to a full stdout pipe that we stopped reading.
    if parse_result.is_err() {
        let _ = child.kill();
    }

    // Always wait for child and join stderr thread, even if parsing failed
    let status: ExitStatus = child.wait().context("Failed to wait for codex process")?;
    let (stderr_buffer, stderr_truncated, stderr_error) =
        stderr_handle.join().expect("stderr thread panicked");

    let output = parse_result.context("Failed to read codex stdout")?;

    let child_exit = status.code().unwrap_or(1);
    // Codex normally exits non-zero when it emits turn.failed / error events,
    // but the JSONL schema does not guarantee this. Escalate so downstream
    // callers (CI, scripts) do not treat an API failure as success.
    let exit_code = if child_exit == 0 && !output.errors.is_empty() {
        1
    } else {
        child_exit
    };

    // On failure, print stderr for debugging
    if !status.success() {
        if !stderr_buffer.is_empty() {
            eprintln!("--- codex stderr ---");
            let _ = io::stderr().write_all(&stderr_buffer);
            if stderr_truncated {
                eprintln!(
                    "(stderr truncated to {} bytes)",
                    STDERR_CAP_BYTES
                );
            }
            if let Some(err) = stderr_error {
                eprintln!("(failed to capture full stderr: {})", err);
            }
            eprintln!("--- end stderr ---");
        } else if let Some(err) = stderr_error {
            eprintln!("--- codex stderr ---");
            eprintln!("Failed to capture stderr: {}", err);
            eprintln!("--- end stderr ---");
        }

        if output.events_recognized == 0 {
            eprintln!("Codex exited with code {} and produced no JSON output", child_exit);
        }
    } else if let Some(err) = stderr_error {
        eprintln!("Warning: Failed to capture codex stderr: {}", err);
    }

    // Print the formatted output
    output.print();

    Ok(exit_code)
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
