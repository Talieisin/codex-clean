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
    },
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
            Some(Event::TurnCompleted {
                input_tokens,
                cached_input_tokens,
                output_tokens,
            })
        }
        _ => None, // Ignore unknown events gracefully
    }
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
        let json = r#"{"type":"turn.completed","usage":{"input_tokens":15228,"cached_input_tokens":14208,"output_tokens":249}}"#;
        let event = extract_event(json).unwrap();
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
            } => {
                assert_eq!(input_tokens, 100);
                assert_eq!(cached_input_tokens, 0);
                assert_eq!(output_tokens, 0);
            }
            _ => panic!("Expected TurnCompleted"),
        }
    }

    #[test]
    fn test_parse_turn_completed_missing_usage() {
        let json = r#"{"type":"turn.completed"}"#;
        assert!(extract_event(json).is_none());
    }
}
