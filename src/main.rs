use std::io::{self, BufRead};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use codex_clean::{runner, seat_cmd};

#[derive(Parser)]
#[command(name = "codex-clean")]
#[command(about = "Wraps codex exec to filter JSON output, showing session IDs, agent messages, token usage, and supporting session resume and code review")]
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
    /// Review code changes
    Review {
        /// Arguments passed through to codex exec review (e.g., --uncommitted, --base main, --commit SHA)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Manage ChatGPT seats (separate OAuth identities) for rotation across usage caps
    Seat {
        #[command(subcommand)]
        action: SeatAction,
    },
}

#[derive(Subcommand)]
enum SeatAction {
    /// Add a new seat (default: device-code login; --import adopts current ~/.codex/auth.json)
    Add {
        /// Seat identifier (used in CODEX_CLEAN_SEAT)
        name: String,
        /// Human-friendly label shown in `seat list`
        #[arg(long)]
        label: Option<String>,
        /// Adopt the existing ~/.codex/auth.json as this seat (no login flow)
        #[arg(long)]
        import: bool,
        /// Use the browser-redirect login flow instead of device-code
        #[arg(long)]
        browser: bool,
    },
    /// List configured seats and their current state
    List,
    /// Re-authenticate an existing seat
    Login {
        /// Name of the seat to re-authenticate
        name: String,
        /// Use the browser-redirect login flow instead of device-code
        #[arg(long)]
        browser: bool,
    },
    /// Pin the active seat for future runs
    Use {
        /// Name of the seat to make active
        name: String,
    },
    /// Remove a seat from the configuration
    Remove {
        /// Name of the seat to remove
        name: String,
        /// Skip the confirmation prompt
        #[arg(long, short)]
        yes: bool,
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
        Some(Commands::Review { args }) => run_review(args),
        Some(Commands::Seat { action }) => run_seat(action).map(|()| 0),
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

fn run_seat(action: SeatAction) -> anyhow::Result<()> {
    match action {
        SeatAction::Add {
            name,
            label,
            import,
            browser,
        } => seat_cmd::add(&name, label.as_deref(), import, browser),
        SeatAction::List => seat_cmd::list(),
        SeatAction::Login { name, browser } => seat_cmd::login(&name, browser),
        SeatAction::Use { name } => seat_cmd::use_seat(&name),
        SeatAction::Remove { name, yes } => seat_cmd::remove(&name, yes),
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

    runner::run_codex(&codex_args.to_vec(), &prompt, runner::Mode::Exec)
}

fn run_resume(
    last: bool,
    session_id: Option<String>,
    prompt: Option<String>,
) -> anyhow::Result<i32> {
    // When --last is used, both positionals are prompt fragments
    // (e.g., `resume --last add error` → prompt "add error")
    let (resume_target, actual_prompt) = if last {
        let parts: Vec<&str> = [session_id.as_deref(), prompt.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        (runner::ResumeTarget::Last, parts.join(" "))
    } else {
        let id = session_id.ok_or_else(|| anyhow::anyhow!("Either --last or SESSION_ID is required"))?;
        (runner::ResumeTarget::SessionId(id), prompt.unwrap_or_default())
    };

    runner::run_codex(&[], &actual_prompt, runner::Mode::Resume(resume_target))
}

fn run_review(args: Vec<String>) -> anyhow::Result<i32> {
    // Pass all args through to codex exec review — it handles its own
    // flag and optional trailing prompt parsing. No heuristic needed.
    runner::run_codex(&args, "", runner::Mode::Review)
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
    fn review_no_args() {
        let cli = Cli::parse_from(["codex-clean", "review"]);
        match cli.command {
            Some(Commands::Review { args }) => {
                assert!(args.is_empty());
            }
            _ => panic!("Expected review command"),
        }
    }

    #[test]
    fn review_with_flags_only() {
        let cli = Cli::parse_from(["codex-clean", "review", "--uncommitted"]);
        match cli.command {
            Some(Commands::Review { args }) => {
                assert_eq!(args, vec!["--uncommitted".to_string()]);
            }
            _ => panic!("Expected review command"),
        }
    }

    #[test]
    fn review_with_flags_and_prompt() {
        let cli = Cli::parse_from([
            "codex-clean",
            "review",
            "--base",
            "main",
            "focus on error handling",
        ]);
        match cli.command {
            Some(Commands::Review { args }) => {
                assert_eq!(
                    args,
                    vec![
                        "--base".to_string(),
                        "main".to_string(),
                        "focus on error handling".to_string(),
                    ]
                );
            }
            _ => panic!("Expected review command"),
        }
    }

    #[test]
    fn resume_last_joins_split_prompt() {
        // Simulates what clap produces for `resume --last add error`
        // (session_id="add", prompt="error")
        // run_resume should join them into "add error"
        let cli = Cli::parse_from([
            "codex-clean",
            "resume",
            "--last",
            "add",
            "error",
        ]);
        match cli.command {
            Some(Commands::Resume {
                last,
                session_id,
                prompt,
            }) => {
                assert!(last);
                assert_eq!(session_id, Some("add".to_string()));
                assert_eq!(prompt, Some("error".to_string()));
                // Verify run_resume would join them — test the logic directly
                let parts: Vec<&str> = [session_id.as_deref(), prompt.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect();
                assert_eq!(parts.join(" "), "add error");
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
