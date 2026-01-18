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

/// Run codex with the given arguments and prompt
pub fn run_codex(args: &[String], prompt: &str, resume: Option<ResumeTarget>) -> Result<i32> {
    let mut cmd = Command::new("codex");

    // Build command based on mode
    // Both modes use "codex exec" with --experimental-json for JSON output
    cmd.arg("exec");
    cmd.arg("--experimental-json");
    cmd.arg("--skip-git-repo-check");

    // Track if we need to send prompt via stdin (required for --last)
    let mut use_stdin_for_prompt = false;

    if let Some(target) = resume {
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
    } else {
        cmd.args(args);
        cmd.arg(prompt);
    }

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    if use_stdin_for_prompt {
        cmd.stdin(Stdio::piped());
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
    let output =
        parse_codex_stream(reader).context("Failed to read codex stdout")?;

    // Wait for process to complete
    let status: ExitStatus = child.wait().context("Failed to wait for codex process")?;
    let (stderr_buffer, stderr_truncated, stderr_error) =
        stderr_handle.join().expect("stderr thread panicked");

    let exit_code = status.code().unwrap_or(1);

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

        if output.session_id.is_none() && output.messages.is_empty() {
            eprintln!("Codex exited with code {} and produced no JSON output", exit_code);
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

        if let Some(event) = extract_event(&line) {
            match event {
                Event::ThreadStarted { thread_id } => {
                    output.add_thread_id(thread_id);
                }
                Event::AgentMessage { text } => {
                    if let Some(t) = text {
                        output.add_message(t);
                    }
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
    fn parse_codex_stream_propagates_errors() {
        // Invalid UTF-8 sequence should trigger an error from lines()
        let data = b"\x80\x80";
        let cursor = Cursor::new(&data[..]);
        let err = parse_codex_stream(BufReader::new(cursor)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
