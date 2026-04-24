// Fixture-driven integration tests using the production parser.

use std::io::{BufReader, Cursor};

use codex_clean::events::{extract_event, Event};
use codex_clean::runner::parse_codex_stream;

fn parse_fixture(name: &str) -> codex_clean::output::CodexOutput {
    let data = std::fs::read_to_string(format!("tests/fixtures/{}", name))
        .unwrap_or_else(|e| panic!("Failed to read fixture {}: {}", name, e));
    let cursor = Cursor::new(data);
    parse_codex_stream(BufReader::new(cursor)).unwrap()
}

#[test]
fn fixture_sample_session() {
    let output = parse_fixture("sample_session.jsonl");

    assert_eq!(
        output.session_id,
        Some("0199a213-81c0-7800-8aa1-bbab2a035a53".to_string())
    );
    assert_eq!(output.messages.len(), 2);
    assert_eq!(output.messages[0], "This is a Rust project. ");
    assert_eq!(output.messages[1], "It uses Cargo for dependency management.");
    assert_eq!(output.usage, Some((5120, 4096, 128, 0)));
}

#[test]
fn fixture_sample_session_renders_usage() {
    let output = parse_fixture("sample_session.jsonl");
    let rendered = output.render();

    assert!(rendered.stdout.contains("Session: 0199a213"));
    assert!(rendered.stdout.contains("This is a Rust project."));
    assert!(rendered.stdout.contains("Tokens: 5120 input (4096 cached), 128 output"));
    assert!(rendered.stderr.is_empty());
}

#[test]
fn fixture_turn_completed() {
    let data = std::fs::read_to_string("tests/fixtures/turn_completed.json").unwrap();
    for line in data.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event = extract_event(line).expect("Should parse turn.completed");
        match event {
            Event::TurnCompleted {
                input_tokens,
                cached_input_tokens,
                output_tokens,
                reasoning_output_tokens: _,
            } => {
                assert_eq!(input_tokens, 15228);
                assert_eq!(cached_input_tokens, 14208);
                assert_eq!(output_tokens, 249);
            }
            _ => panic!("Expected TurnCompleted event"),
        }
    }
}

#[test]
fn fixture_failed_turn() {
    let output = parse_fixture("failed_turn.jsonl");
    assert_eq!(
        output.session_id,
        Some("0199c789-failed-session-id".to_string())
    );
    assert!(output.messages.is_empty());
    assert_eq!(output.errors.len(), 1);
    assert!(output.errors[0].contains("invalid_request_error"));

    let rendered = output.render();
    assert!(rendered.stderr.contains("Error from codex"));
    assert!(rendered.stderr.contains("invalid_request_error"));
}

#[test]
fn fixture_review_session() {
    let output = parse_fixture("review_session.jsonl");

    assert_eq!(
        output.session_id,
        Some("0199b456-review-session-id".to_string())
    );
    assert_eq!(output.messages.len(), 1);
    assert!(output.messages[0].contains("Code Review"));
    assert_eq!(output.usage, Some((8192, 6144, 512, 0)));
}

#[test]
fn unknown_events_are_silently_skipped() {
    // The sample_session fixture contains reasoning, command_execution,
    // turn.started etc. — only agent_message items should appear.
    let output = parse_fixture("sample_session.jsonl");
    assert_eq!(output.messages.len(), 2);
}

/// Regression test for the stdin hang.
///
/// codex >= 0.123 reads additional input from stdin until EOF. If codex-clean
/// inherits a long-lived stdin pipe from its parent (e.g. Claude Code), codex
/// blocks forever waiting for EOF. The fix is to pass `Stdio::null()` to the
/// child. This test exercises the scenario by running codex-clean under a
/// parent pipe we deliberately keep open, with a fake `codex` shim that
/// drains stdin before emitting any JSONL.
#[cfg(unix)]
#[test]
fn does_not_hang_when_parent_stdin_stays_open() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let tmp = env!("CARGO_TARGET_TMPDIR");
    let shim_dir = std::path::PathBuf::from(tmp).join("codex-shim-stdin");
    std::fs::create_dir_all(&shim_dir).unwrap();

    let shim = shim_dir.join("codex");
    {
        let mut f = std::fs::File::create(&shim).unwrap();
        // Drains stdin until EOF (mirrors codex's stdin-reading behaviour),
        // then emits a minimal valid JSONL stream.
        f.write_all(
            b"#!/bin/sh\n\
              cat >/dev/null\n\
              printf '%s\\n' '{\"type\":\"thread.started\",\"thread_id\":\"shim-session\"}'\n\
              printf '%s\\n' '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"ok\"}}'\n\
              printf '%s\\n' '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1,\"cached_input_tokens\":0,\"output_tokens\":1,\"reasoning_output_tokens\":0}}'\n",
        )
        .unwrap();
        let mut perms = f.metadata().unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&shim, perms).unwrap();
    }

    let orig_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", shim_dir.display(), orig_path);

    let binary = env!("CARGO_BIN_EXE_codex-clean");

    let mut child = Command::new(binary)
        .env("PATH", &new_path)
        .arg("hello")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn codex-clean");

    // Hold the write end of stdin open for the lifetime of the test — this is
    // what simulates an orchestration parent with a long-lived stdin pipe.
    // If the Stdio::null() fix regresses, the shim will block on `cat >/dev/null`
    // and codex-clean will hang reading its stdout until we time out.
    let _stdin = child.stdin.take().expect("stdin was piped");

    let deadline = Duration::from_secs(10);
    let start = Instant::now();
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                assert!(
                    status.success(),
                    "codex-clean exited with failure: {:?}",
                    status
                );
                return;
            }
            None => {
                if start.elapsed() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "codex-clean hung for >{:?} with an open parent stdin — \
                         the Stdio::null() fix in runner.rs has likely regressed",
                        deadline
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}
