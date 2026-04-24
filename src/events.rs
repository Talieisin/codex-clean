use serde_json::Value;

/// Events we care about from codex JSON output
#[derive(Debug, Clone)]
pub enum Event {
    ThreadStarted { thread_id: String },
    AgentMessage { text: Option<String> },
    TurnCompleted {
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
        reasoning_output_tokens: u64,
    },
    TurnFailed { message: String },
    StreamError { message: String },
}

/// Parse a JSON line permissively, extracting only events we care about.
/// Returns None for unknown/malformed events (which we silently skip).
pub fn extract_event(line: &str) -> Option<Event> {
    let v: Value = serde_json::from_str(line).ok()?;
    let event_type = v.get("type")?.as_str()?;

    match event_type {
        "thread.started" => {
            let thread_id = v.get("thread_id")?.as_str()?.to_string();
            Some(Event::ThreadStarted { thread_id })
        }
        "item.completed" => {
            let item = v.get("item")?;
            if item.get("type")?.as_str()? == "agent_message" {
                let text = item.get("text").and_then(|t| t.as_str()).map(String::from);
                Some(Event::AgentMessage { text })
            } else {
                None
            }
        }
        "turn.completed" => {
            let usage = v.get("usage")?;
            let input_tokens = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let cached_input_tokens = usage.get("cached_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let output_tokens = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let reasoning_output_tokens = usage
                .get("reasoning_output_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Some(Event::TurnCompleted {
                input_tokens,
                cached_input_tokens,
                output_tokens,
                reasoning_output_tokens,
            })
        }
        "turn.failed" => {
            let message = extract_error_message(v.get("error"));
            Some(Event::TurnFailed { message })
        }
        "error" => {
            let message = v
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown stream error")
                .to_string();
            Some(Event::StreamError { message })
        }
        _ => None, // Ignore unknown events gracefully
    }
}

/// Extract a human-readable error message from a codex `error` field, which
/// can show up as several shapes across codex versions:
///   - missing entirely
///   - a bare string: `"error": "something went wrong"`
///   - an object with a `message` key: `{"message": "..."}`
///   - an object without `message` but with useful keys like `code` / `type`
fn extract_error_message(err: Option<&Value>) -> String {
    let Some(err) = err else {
        return "turn failed (no error field)".to_string();
    };
    if let Some(s) = err.as_str() {
        return s.to_string();
    }
    if let Some(m) = err.get("message").and_then(|m| m.as_str()) {
        return m.to_string();
    }
    if err.as_object().is_some_and(|o| !o.is_empty()) {
        return err.to_string();
    }
    "turn failed (no message)".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_thread_started() {
        let json = r#"{"type":"thread.started","thread_id":"0199a213-81c0-7800-8aa1-bbab2a035a53"}"#;
        let event = extract_event(json).unwrap();
        match event {
            Event::ThreadStarted { thread_id } => {
                assert_eq!(thread_id, "0199a213-81c0-7800-8aa1-bbab2a035a53");
            }
            _ => panic!("Expected ThreadStarted"),
        }
    }

    #[test]
    fn test_parse_agent_message() {
        let json = r#"{"type":"item.completed","item":{"type":"agent_message","text":"Hello world"}}"#;
        let event = extract_event(json).unwrap();
        match event {
            Event::AgentMessage { text } => {
                assert_eq!(text, Some("Hello world".to_string()));
            }
            _ => panic!("Expected AgentMessage"),
        }
    }

    #[test]
    fn test_parse_agent_message_no_text() {
        let json = r#"{"type":"item.completed","item":{"type":"agent_message"}}"#;
        let event = extract_event(json).unwrap();
        match event {
            Event::AgentMessage { text } => {
                assert!(text.is_none());
            }
            _ => panic!("Expected AgentMessage"),
        }
    }

    #[test]
    fn test_ignore_unknown_event() {
        let json = r#"{"type":"unknown.event","data":"something"}"#;
        assert!(extract_event(json).is_none());
    }

    #[test]
    fn test_ignore_non_agent_item() {
        let json = r#"{"type":"item.completed","item":{"type":"tool_call","name":"read"}}"#;
        assert!(extract_event(json).is_none());
    }

    #[test]
    fn test_ignore_malformed_json() {
        let json = r#"not valid json at all"#;
        assert!(extract_event(json).is_none());
    }

    #[test]
    fn test_ignore_missing_type() {
        let json = r#"{"data":"no type field"}"#;
        assert!(extract_event(json).is_none());
    }

    #[test]
    fn test_parse_turn_completed() {
        let json = r#"{"type":"turn.completed","usage":{"input_tokens":15228,"cached_input_tokens":14208,"output_tokens":249,"reasoning_output_tokens":64}}"#;
        let event = extract_event(json).unwrap();
        match event {
            Event::TurnCompleted {
                input_tokens,
                cached_input_tokens,
                output_tokens,
                reasoning_output_tokens,
            } => {
                assert_eq!(input_tokens, 15228);
                assert_eq!(cached_input_tokens, 14208);
                assert_eq!(output_tokens, 249);
                assert_eq!(reasoning_output_tokens, 64);
            }
            _ => panic!("Expected TurnCompleted"),
        }
    }

    #[test]
    fn test_parse_turn_completed_partial_usage() {
        let json = r#"{"type":"turn.completed","usage":{"input_tokens":100}}"#;
        let event = extract_event(json).unwrap();
        match event {
            Event::TurnCompleted {
                input_tokens,
                cached_input_tokens,
                output_tokens,
                reasoning_output_tokens,
            } => {
                assert_eq!(input_tokens, 100);
                assert_eq!(cached_input_tokens, 0);
                assert_eq!(output_tokens, 0);
                assert_eq!(reasoning_output_tokens, 0);
            }
            _ => panic!("Expected TurnCompleted"),
        }
    }

    #[test]
    fn test_parse_turn_completed_missing_usage() {
        let json = r#"{"type":"turn.completed"}"#;
        assert!(extract_event(json).is_none());
    }

    #[test]
    fn test_parse_turn_failed() {
        let json = r#"{"type":"turn.failed","error":{"message":"invalid_request_error: bad reasoning effort"}}"#;
        let event = extract_event(json).unwrap();
        match event {
            Event::TurnFailed { message } => {
                assert!(message.contains("invalid_request_error"));
            }
            _ => panic!("Expected TurnFailed"),
        }
    }

    #[test]
    fn test_parse_turn_failed_missing_message() {
        let json = r#"{"type":"turn.failed","error":{}}"#;
        let event = extract_event(json).unwrap();
        match event {
            Event::TurnFailed { message } => {
                assert_eq!(message, "turn failed (no message)");
            }
            _ => panic!("Expected TurnFailed"),
        }
    }

    #[test]
    fn test_parse_turn_failed_string_error() {
        let json = r#"{"type":"turn.failed","error":"rate limit exceeded"}"#;
        let event = extract_event(json).unwrap();
        match event {
            Event::TurnFailed { message } => assert_eq!(message, "rate limit exceeded"),
            _ => panic!("Expected TurnFailed"),
        }
    }

    #[test]
    fn test_parse_turn_failed_object_without_message() {
        let json = r#"{"type":"turn.failed","error":{"code":"E_RATE_LIMIT","type":"rate_limit"}}"#;
        let event = extract_event(json).unwrap();
        match event {
            Event::TurnFailed { message } => {
                assert!(message.contains("E_RATE_LIMIT"));
                assert!(message.contains("rate_limit"));
            }
            _ => panic!("Expected TurnFailed"),
        }
    }

    #[test]
    fn test_parse_turn_failed_no_error_field() {
        let json = r#"{"type":"turn.failed"}"#;
        let event = extract_event(json).unwrap();
        match event {
            Event::TurnFailed { message } => {
                assert_eq!(message, "turn failed (no error field)");
            }
            _ => panic!("Expected TurnFailed"),
        }
    }

    #[test]
    fn test_parse_stream_error() {
        let json = r#"{"type":"error","message":"connection reset"}"#;
        let event = extract_event(json).unwrap();
        match event {
            Event::StreamError { message } => {
                assert_eq!(message, "connection reset");
            }
            _ => panic!("Expected StreamError"),
        }
    }
}
