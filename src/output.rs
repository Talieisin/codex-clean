use std::fmt::Write as FmtWrite;

/// Collected results from parsing codex output
#[derive(Debug, Default)]
pub struct CodexOutput {
    pub session_id: Option<String>,
    pub messages: Vec<String>,
    pub multiple_threads_seen: bool,
    /// Token usage: (input_tokens, cached_input_tokens, output_tokens, reasoning_output_tokens)
    pub usage: Option<(u64, u64, u64, u64)>,
    /// Number of non-empty lines received from stdout
    pub lines_seen: usize,
    /// Number of lines that matched a recognised event
    pub events_recognized: usize,
    /// Errors surfaced by codex via `turn.failed` or stream `error` events
    pub errors: Vec<String>,
}

/// Rendered stdout/stderr strings
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RenderedOutput {
    pub stdout: String,
    pub stderr: String,
}

fn normalize_error_key(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl CodexOutput {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a thread ID. Uses first seen, warns if multiple.
    pub fn add_thread_id(&mut self, thread_id: String) {
        if self.session_id.is_none() {
            self.session_id = Some(thread_id);
        } else if self.session_id.as_ref() != Some(&thread_id) {
            self.multiple_threads_seen = true;
        }
    }

    /// Record token usage (uses last seen values)
    pub fn add_usage(&mut self, input: u64, cached: u64, output: u64, reasoning: u64) {
        self.usage = Some((input, cached, output, reasoning));
    }

    /// Add an agent message text
    pub fn add_message(&mut self, text: String) {
        if !text.is_empty() {
            self.messages.push(text);
        }
    }

    /// Record an error surfaced by codex (turn.failed or stream error).
    /// Deduped — codex often emits the same error via both an `error`
    /// event and a `turn.failed` event. Comparison is whitespace-normalised
    /// so that trivial formatting drift between the two shapes does not
    /// bypass the dedupe.
    pub fn add_error(&mut self, message: String) {
        let key = normalize_error_key(&message);
        if key.is_empty() {
            return;
        }
        if !self.errors.iter().any(|m| normalize_error_key(m) == key) {
            self.errors.push(message);
        }
    }

    /// Get the aggregated message content
    pub fn aggregated_message(&self) -> String {
        self.messages.join("\n")
    }

    /// Compose stdout/stderr strings for printing
    pub fn render(&self) -> RenderedOutput {
        let mut stdout = String::new();
        let mut stderr = String::new();

        if self.multiple_threads_seen {
            let _ = writeln!(stderr, "Warning: Multiple thread IDs seen, using first");
        }

        match &self.session_id {
            Some(id) => {
                let _ = writeln!(stdout, "Session: {}", id);
            }
            None => {
                let _ = writeln!(stderr, "Warning: No session ID received");
            }
        }

        if self.lines_seen > 0 && self.events_recognized == 0 {
            let _ = writeln!(
                stderr,
                "Warning: Received {} lines from codex but none matched known event types \
                 (possible schema change in upstream codex)",
                self.lines_seen
            );
        }

        let message = self.aggregated_message();
        if message.is_empty() {
            if self.session_id.is_some() && self.errors.is_empty() {
                let _ = writeln!(stderr, "Note: No response received");
            }
        } else {
            let _ = writeln!(stdout);
            let _ = writeln!(stdout, "{}", message);
        }

        for err in &self.errors {
            let _ = writeln!(stderr, "Error from codex: {}", err);
        }

        if let Some((input, cached, output, reasoning)) = self.usage {
            let _ = writeln!(stdout);
            if reasoning > 0 {
                let _ = writeln!(
                    stdout,
                    "Tokens: {} input ({} cached), {} output ({} reasoning)",
                    input, cached, output, reasoning
                );
            } else {
                let _ = writeln!(
                    stdout,
                    "Tokens: {} input ({} cached), {} output",
                    input, cached, output
                );
            }
        }

        RenderedOutput { stdout, stderr }
    }

    /// Format and print the output
    pub fn print(&self) {
        let rendered = self.render();
        if !rendered.stdout.is_empty() {
            print!("{}", rendered.stdout);
        }
        if !rendered.stderr.is_empty() {
            eprint!("{}", rendered.stderr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_first_thread_id_wins() {
        let mut output = CodexOutput::new();
        output.add_thread_id("first".to_string());
        output.add_thread_id("second".to_string());
        assert_eq!(output.session_id, Some("first".to_string()));
        assert!(output.multiple_threads_seen);
    }

    #[test]
    fn test_same_thread_id_no_warning() {
        let mut output = CodexOutput::new();
        output.add_thread_id("same".to_string());
        output.add_thread_id("same".to_string());
        assert_eq!(output.session_id, Some("same".to_string()));
        assert!(!output.multiple_threads_seen);
    }

    #[test]
    fn test_message_aggregation() {
        let mut output = CodexOutput::new();
        output.add_message("Hello".to_string());
        output.add_message("world".to_string());
        assert_eq!(output.aggregated_message(), "Hello\nworld");
    }

    #[test]
    fn test_empty_messages_skipped() {
        let mut output = CodexOutput::new();
        output.add_message("".to_string());
        output.add_message("content".to_string());
        output.add_message("".to_string());
        assert_eq!(output.messages.len(), 1);
        assert_eq!(output.aggregated_message(), "content");
    }

    #[test]
    fn render_warns_on_multiple_threads() {
        let mut output = CodexOutput::new();
        output.multiple_threads_seen = true;
        output.session_id = Some("abc".into());
        let rendered = output.render();
        assert!(rendered.stderr.contains("Multiple thread IDs"));
        assert!(rendered.stdout.contains("Session: abc"));
    }

    #[test]
    fn render_handles_missing_session_and_empty_message() {
        let output = CodexOutput::new();
        let rendered = output.render();
        assert!(rendered.stderr.contains("No session ID"));
        assert!(!rendered.stderr.contains("No response"));
        assert!(rendered.stdout.is_empty());
    }

    #[test]
    fn add_usage_stores_last_seen() {
        let mut output = CodexOutput::new();
        output.add_usage(100, 50, 25, 10);
        assert_eq!(output.usage, Some((100, 50, 25, 10)));
        output.add_usage(200, 150, 75, 30);
        assert_eq!(output.usage, Some((200, 150, 75, 30)));
    }

    #[test]
    fn render_includes_usage_line() {
        let mut output = CodexOutput::new();
        output.session_id = Some("abc".into());
        output.add_message("hello".into());
        output.add_usage(15228, 14208, 249, 0);
        let rendered = output.render();
        assert!(rendered.stdout.contains("Tokens: 15228 input (14208 cached), 249 output"));
        assert!(!rendered.stdout.contains("reasoning"));
    }

    #[test]
    fn render_includes_reasoning_tokens_when_nonzero() {
        let mut output = CodexOutput::new();
        output.session_id = Some("abc".into());
        output.add_message("hello".into());
        output.add_usage(15228, 14208, 249, 512);
        let rendered = output.render();
        assert!(rendered
            .stdout
            .contains("Tokens: 15228 input (14208 cached), 249 output (512 reasoning)"));
    }

    #[test]
    fn add_error_dedupes_whitespace_variants() {
        let mut output = CodexOutput::new();
        output.add_error("connection reset".to_string());
        output.add_error("  connection   reset  ".to_string());
        output.add_error("connection\nreset".to_string());
        assert_eq!(output.errors.len(), 1, "whitespace-only differences should dedupe");
    }

    #[test]
    fn add_error_skips_empty() {
        let mut output = CodexOutput::new();
        output.add_error("".to_string());
        output.add_error("   \n  ".to_string());
        assert!(output.errors.is_empty());
    }

    #[test]
    fn render_surfaces_turn_failed_error() {
        let mut output = CodexOutput::new();
        output.session_id = Some("abc".into());
        output.add_error("invalid_request_error: bad reasoning effort".into());
        let rendered = output.render();
        assert!(rendered.stderr.contains("Error from codex"));
        assert!(rendered.stderr.contains("invalid_request_error"));
        // Should not claim "No response received" — the error already explains it
        assert!(!rendered.stderr.contains("No response received"));
    }

    #[test]
    fn render_omits_usage_when_none() {
        let mut output = CodexOutput::new();
        output.session_id = Some("abc".into());
        output.add_message("hello".into());
        let rendered = output.render();
        assert!(!rendered.stdout.contains("Tokens:"));
    }

    #[test]
    fn render_warns_on_unrecognized_lines() {
        let mut output = CodexOutput::new();
        output.lines_seen = 5;
        output.events_recognized = 0;
        let rendered = output.render();
        assert!(rendered.stderr.contains("none matched known event types"));
        assert!(rendered.stderr.contains("5 lines"));
    }

    #[test]
    fn render_no_warning_when_some_events_recognized() {
        let mut output = CodexOutput::new();
        output.lines_seen = 5;
        output.events_recognized = 2;
        output.session_id = Some("abc".into());
        output.add_message("hello".into());
        let rendered = output.render();
        assert!(!rendered.stderr.contains("none matched"));
    }

    #[test]
    fn render_no_warning_when_no_lines() {
        let output = CodexOutput::new();
        let rendered = output.render();
        assert!(!rendered.stderr.contains("none matched"));
    }
}
