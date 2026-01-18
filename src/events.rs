use serde_json::Value;

/// Events we care about from codex JSON output
#[derive(Debug, Clone)]
pub enum Event {
    ThreadStarted { thread_id: String },
    AgentMessage { text: Option<String> },
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
}
