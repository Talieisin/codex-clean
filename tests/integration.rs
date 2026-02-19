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
    assert_eq!(output.usage, Some((5120, 4096, 128)));
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
fn fixture_review_session() {
    let output = parse_fixture("review_session.jsonl");

    assert_eq!(
        output.session_id,
        Some("0199b456-review-session-id".to_string())
    );
    assert_eq!(output.messages.len(), 1);
    assert!(output.messages[0].contains("Code Review"));
    assert_eq!(output.usage, Some((8192, 6144, 512)));
}

#[test]
fn unknown_events_are_silently_skipped() {
    // The sample_session fixture contains reasoning, command_execution,
    // turn.started etc. — only agent_message items should appear.
    let output = parse_fixture("sample_session.jsonl");
    assert_eq!(output.messages.len(), 2);
}
