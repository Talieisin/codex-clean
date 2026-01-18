use std::fmt::Write as FmtWrite;

/// Collected results from parsing codex output
#[derive(Debug, Default)]
pub struct CodexOutput {
    pub session_id: Option<String>,
    pub messages: Vec<String>,
    pub multiple_threads_seen: bool,
}

/// Rendered stdout/stderr strings
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RenderedOutput {
    pub stdout: String,
    pub stderr: String,
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

    /// Add an agent message text
    pub fn add_message(&mut self, text: String) {
        if !text.is_empty() {
            self.messages.push(text);
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

        let message = self.aggregated_message();
        if message.is_empty() {
            if self.session_id.is_some() {
                let _ = writeln!(stderr, "Note: No response received");
            }
        } else {
            let _ = writeln!(stdout);
            let _ = writeln!(stdout, "{}", message);
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
}
