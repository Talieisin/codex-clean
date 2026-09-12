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
    /// Estimated credit cost of a finished session
    Cost {
        /// Session (thread) id; omit with --last
        session_id: Option<String>,
        /// Use the most recent session this wrapper ran
        #[arg(long)]
        last: bool,
        /// Seat that ran it (default: inferred)
        #[arg(long, value_name = "NAME")]
        seat: Option<String>,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
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
    /// List configured seats and their current state (offline; shows the last recorded usage)
    List,
    /// Query live usage for each seat via `codex app-server` and record it in state.json
    Status {
        /// Only this seat
        name: Option<String>,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
        /// Clear the recorded cooldown for this seat (the only way `status` removes one)
        #[arg(long, value_name = "NAME")]
        clear_cooldown: Option<String>,
        /// Also show account token activity (one extra call per seat)
        #[arg(long)]
        usage: bool,
    },
    /// Redeem a free usage-limit reset (codex grants these; they expire unused)
    Reset {
        /// Seat to reset (default: the seat a run would pick)
        name: Option<String>,
        /// Redeem this specific grant (see --dry-run for ids)
        #[arg(long, value_name = "ID")]
        credit_id: Option<String>,
        /// List the available resets without redeeming one
        #[arg(long)]
        dry_run: bool,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// Show or set the rotation strategy: least-recently-used (lru), round-robin (rr), fixed <seat>, balanced
    Strategy {
        /// Strategy name; omit to show the current one
        name: Option<String>,
        /// Seat to prefer (only for `fixed`)
        seat: Option<String>,
    },
    /// Show or set whether runs may spend workspace credits once included quota is used up
    Credits {
        /// ask (default) | never | always | allow (until quota resets) | revoke; omit to show
        action: Option<String>,
    },
    /// Show the seat event log (limits hit, auth failures, cooldowns, orphaned blobs, logins)
    Events {
        /// Number of most recent entries to show
        #[arg(long, default_value_t = 20)]
        tail: usize,
    },
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
        Some(Commands::Seat { action }) => run_seat(action),
        Some(Commands::Cost {
            session_id,
            last,
            seat,
            json,
        }) => seat_cmd::cost(session_id.as_deref(), last, seat.as_deref(), json),
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

fn run_seat(action: SeatAction) -> anyhow::Result<i32> {
    match action {
        SeatAction::Add {
            name,
            label,
            import,
            browser,
        } => seat_cmd::add(&name, label.as_deref(), import, browser).map(|()| 0),
        SeatAction::List => seat_cmd::list().map(|()| 0),
        SeatAction::Status {
            name,
            json,
            clear_cooldown,
            usage,
        } => seat_cmd::status(name.as_deref(), json, clear_cooldown.as_deref(), usage),
        SeatAction::Reset {
            name,
            credit_id,
            dry_run,
            json,
        } => seat_cmd::reset(name.as_deref(), credit_id.as_deref(), dry_run, json),
        SeatAction::Strategy { name, seat } => {
            seat_cmd::strategy(name.as_deref(), seat.as_deref()).map(|()| 0)
        }
        SeatAction::Credits { action } => seat_cmd::credits(action.as_deref()).map(|()| 0),
        SeatAction::Events { tail } => seat_cmd::events(tail).map(|()| 0),
        SeatAction::Login { name, browser } => seat_cmd::login(&name, browser).map(|()| 0),
        SeatAction::Use { name } => seat_cmd::use_seat(&name).map(|()| 0),
        SeatAction::Remove { name, yes } => seat_cmd::remove(&name, yes).map(|()| 0),
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

    runner::run_codex(
        &codex_args.to_vec(),
        &prompt,
        runner::Mode::Exec,
        interactive_session(prompt_arg == "-"),
    )
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

    runner::run_codex(
        &[],
        &actual_prompt,
        runner::Mode::Resume(resume_target),
        interactive_session(false),
    )
}

fn run_review(args: Vec<String>) -> anyhow::Result<i32> {
    // Pass all args through to codex exec review — it handles its own
    // flag and optional trailing prompt parsing. No heuristic needed.
    runner::run_codex(&args, "", runner::Mode::Review, interactive_session(false))
}

/// Whether a credits prompt may be shown: stdin and stderr are terminals,
/// the prompt was not read from stdin, and CODEX_CLEAN_NONINTERACTIVE is not `1`.
fn interactive_session(prompt_from_stdin: bool) -> bool {
    use std::io::IsTerminal;
    interactive_from(
        prompt_from_stdin,
        io::stdin().is_terminal(),
        io::stderr().is_terminal(),
        std::env::var("CODEX_CLEAN_NONINTERACTIVE").ok().as_deref(),
    )
}

fn interactive_from(
    prompt_from_stdin: bool,
    stdin_tty: bool,
    stderr_tty: bool,
    noninteractive_env: Option<&str>,
) -> bool {
    !prompt_from_stdin && stdin_tty && stderr_tty && noninteractive_env != Some("1")
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
    fn interactive_requires_terminals_and_an_argv_prompt() {
        assert!(interactive_from(false, true, true, None));
        assert!(!interactive_from(true, true, true, None), "prompt read from stdin");
        assert!(!interactive_from(false, false, true, None), "stdin not a terminal");
        assert!(!interactive_from(false, true, false, None), "stderr not a terminal");
        assert!(!interactive_from(false, true, true, Some("1")), "explicitly non-interactive");
        assert!(interactive_from(false, true, true, Some("0")));
    }

    #[test]
    fn exit_code_from_child_rejects_out_of_range() {
        assert_eq!(exit_code_from_child(-1), ExitCode::FAILURE);
        assert_eq!(exit_code_from_child(256), ExitCode::FAILURE);
        assert_eq!(exit_code_from_child(42), ExitCode::from(42));
    }
}
