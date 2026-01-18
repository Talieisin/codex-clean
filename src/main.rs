mod events;
mod output;
mod runner;

use std::io::{self, BufRead};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "codex-clean")]
#[command(about = "Wraps codex exec to filter JSON output, showing only session IDs and agent messages")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Arguments to pass to codex exec (e.g., -m gpt-5.2-codex --sandbox read-only)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Resume an existing session
    Resume {
        /// Use the most recent session
        #[arg(long)]
        last: bool,

        /// Session ID to resume (optional if --last is used)
        session_id: Option<String>,

        /// Optional prompt for the resumed session
        #[arg(allow_hyphen_values = true)]
        prompt: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let result = match cli.command {
        Some(Commands::Resume {
            last,
            session_id,
            prompt,
        }) => run_resume(last, session_id, prompt),
        None => run_exec(cli.args),
    };

    match result {
        Ok(code) => exit_code_from_child(code),
        Err(e) => {
            eprintln!("Error: {:#}", e);
            ExitCode::from(1)
        }
    }
}

fn run_exec(args: Vec<String>) -> anyhow::Result<i32> {
    let (codex_args, prompt_arg) = split_codex_args(&args)?;

    // Handle stdin input
    let prompt = if prompt_arg == "-" {
        read_stdin()?
    } else {
        prompt_arg.clone()
    };

    if prompt.trim().is_empty() {
        anyhow::bail!("Empty prompt provided");
    }

    runner::run_codex(&codex_args.to_vec(), &prompt, None)
}

fn run_resume(
    last: bool,
    session_id: Option<String>,
    prompt: Option<String>,
) -> anyhow::Result<i32> {
    // When --last is used, the first positional (session_id) is actually the prompt
    let (resume_target, actual_prompt) = if last {
        // With --last, session_id positional becomes the prompt
        (runner::ResumeTarget::Last, session_id.unwrap_or_default())
    } else {
        let id = session_id.ok_or_else(|| anyhow::anyhow!("Either --last or SESSION_ID is required"))?;
        (runner::ResumeTarget::SessionId(id), prompt.unwrap_or_default())
    };

    runner::run_codex(&[], &actual_prompt, Some(resume_target))
}

fn read_stdin() -> anyhow::Result<String> {
    let stdin = io::stdin();
    let mut lines = Vec::new();
    for line in stdin.lock().lines() {
        lines.push(line?);
    }
    Ok(lines.join("\n"))
}

fn split_codex_args<'a>(args: &'a [String]) -> anyhow::Result<(&'a [String], &'a String)> {
    if args.is_empty() {
        anyhow::bail!(
            "Usage: codex-clean [ARGS...] <prompt>\n\nNo prompt provided. Use '-' to read from stdin."
        );
    }

    let (codex_args, prompt_arg) = args.split_at(args.len() - 1);
    let prompt_arg = &prompt_arg[0];

    ensure_valid_prompt(prompt_arg)?;

    Ok((codex_args, prompt_arg))
}

fn ensure_valid_prompt(prompt_arg: &str) -> anyhow::Result<()> {
    if prompt_arg != "-" && prompt_arg.starts_with('-') {
        anyhow::bail!(
            "The final argument ('{}') looks like a flag. Provide a prompt or terminate codex args with '--'.",
            prompt_arg
        );
    }
    Ok(())
}

fn exit_code_from_child(code: i32) -> ExitCode {
    if code < 0 || code > u8::MAX as i32 {
        ExitCode::FAILURE
    } else {
        ExitCode::from(code as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_codex_args_rejects_flag_prompt() {
        let args = vec!["--sandbox".to_string()];
        let err = split_codex_args(&args).unwrap_err();
        assert!(err.to_string().contains("looks like a flag"));
    }

    #[test]
    fn split_codex_args_allows_stdin_marker() {
        let args = vec!["--foo".to_string(), "-".to_string()];
        let (codex_args, prompt) = split_codex_args(&args).unwrap();
        assert_eq!(codex_args, &["--foo".to_string()][..]);
        assert_eq!(prompt, "-");
    }

    #[test]
    fn resume_prompt_accepts_hyphen() {
        let cli = Cli::parse_from([
            "codex-clean",
            "resume",
            "session-123",
            "-leading",
        ]);

        match cli.command {
            Some(Commands::Resume {
                session_id,
                prompt,
                ..
            }) => {
                assert_eq!(session_id, Some("session-123".to_string()));
                assert_eq!(prompt, Some("-leading".to_string()));
            }
            _ => panic!("Expected resume command"),
        }
    }

    #[test]
    fn exit_code_from_child_rejects_out_of_range() {
        assert_eq!(exit_code_from_child(-1), ExitCode::FAILURE);
        assert_eq!(exit_code_from_child(256), ExitCode::FAILURE);
        assert_eq!(exit_code_from_child(42), ExitCode::from(42));
    }
}
