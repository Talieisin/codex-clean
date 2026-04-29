# codex-clean

A Rust CLI wrapper for `codex exec` that filters JSON output, suppressing stderr (thinking tokens) and extracting only session IDs, final agent messages, and token usage stats. Optionally manages multiple ChatGPT seats and rotates between them automatically when one hits its weekly usage cap.

## Installation

```bash
# From source
cargo install --path .

# Or build manually
cargo build --release
# Binary at: target/release/codex-clean
```

## Usage

### Basic Execution

```bash
# Run codex with a prompt
codex-clean "summarize this repo"

# With codex options
codex-clean -m gpt-5.5 --sandbox read-only "explain the main function"

# With config options
codex-clean -m gpt-5.5 --config model_reasoning_effort="high" --sandbox read-only "review this code"

# Change working directory
codex-clean -C /path/to/project "analyze the codebase"

# Read prompt from stdin
echo "what does this code do?" | codex-clean -
```

### Resume Sessions

```bash
# Resume a specific session
codex-clean resume 0199a213-81c0-7800-8aa1-bbab2a035a53 "add error handling"

# Resume the most recent session
codex-clean resume --last "continue with tests"
```

### Review Code Changes

```bash
# Review uncommitted changes (no prompt required)
codex-clean review --uncommitted

# Review changes against a base branch
codex-clean review --base main

# Review a specific commit
codex-clean review --commit abc1234

# Review with a focus prompt
codex-clean review --base main "focus on error handling"

# Review with model options
codex-clean review -m gpt-5.5 --uncommitted
```

### Multi-seat (rotate across multiple ChatGPT accounts)

If you have more than one ChatGPT seat (e.g. a Personal Plus and a Work Pro plan), `codex-clean` can keep one OAuth blob per seat in a private side store and atomically swap the active `~/.codex/auth.json` before each run. When a seat is rate-limited, the next run automatically falls back to the other seat. Sessions stay shared across seats.

```bash
# 1. Adopt your existing login as the first seat
codex-clean seat add personal --import --label "Personal Plus"

# 2. Add a second account via device-code login (no need for two browser profiles)
codex-clean seat add work --label "Work Pro"
# codex prints a URL + 6-char code — open in any browser, sign in to the OTHER ChatGPT account

# 3. List configured seats and their current state
codex-clean seat list

# 4. Use as normal — rotation is automatic (least-recently-used by default)
codex-clean "say hi"

# 5. Pin a specific seat for one invocation (bypasses rotation)
CODEX_CLEAN_SEAT=work codex-clean "say hi"

# 6. Pin the seat across multiple runs (export it in your shell)
export CODEX_CLEAN_SEAT=work
codex-clean "say hi"     # always 'work' until you `unset`
unset CODEX_CLEAN_SEAT

# 7. Pre-position ~/.codex/auth.json for a specific seat (mainly useful before
#    running plain `codex` — does NOT disable rotation for codex-clean)
codex-clean seat use personal

# 8. Re-authenticate a seat whose refresh token expired
codex-clean seat login work

# 9. Remove a seat (deletes its private auth.json)
codex-clean seat remove work
```

> **Pinning vs. switching.** `CODEX_CLEAN_SEAT=<name>` is the only mechanism that bypasses rotation — it applies for as long as the env var is set. `seat use <name>` is a one-shot helper that swaps `~/.codex/auth.json` to that seat's blob right now and updates the recorded active seat; it does not disable rotation, so the *next* `codex-clean` run will re-pick via the rotation policy (LRU by default) as usual. Use `seat use` mainly when you want plain `codex` (not codex-clean) to hit a specific account.

**How rotation works.** Before each codex invocation, `codex-clean` acquires a per-host advisory lock, picks a seat (LRU or round-robin), atomically copies that seat's auth blob into `~/.codex/auth.json`, runs codex, then copies any token refresh codex performed back into the seat's slot. If the run hits the rate-limit message, the seat is cooled until the time codex itself reported, and the next eligible seat is tried. If all seats are cooling, exits with status 75 (`EX_TEMPFAIL`) so callers can branch on it.

**Safety.** Auth files are written `0600` and seat directories `0700` on Unix; writes are atomic (temp file + rename + parent fsync); concurrent codex-clean invocations serialise via `~/.config/codex-clean/codex.lock`; on `seat login`, codex's `tokens.account_id` is verified against the seat's stored value and a mismatch refuses to overwrite (so a slip-of-the-finger doesn't silently stash the wrong account in the wrong slot). Login flows run codex against an isolated temp `CODEX_HOME` so a Ctrl-C never leaves `~/.codex/auth.json` half-replaced.

**Backwards compatibility.** With no `seats.toml` present (i.e. you've never run `seat add`), `codex-clean` behaves exactly as before — no auth swaps, no lock, just a passthrough wrapper.

Layout on disc:

```
~/.codex/                              (codex's own home — unchanged)
  auth.json                            (active seat's tokens; swapped before each run)
  config.toml                          (cli_auth_credentials_store = "file" enforced)
  sessions/, state_5.sqlite, ...       (shared across seats)

~/.config/codex-clean/                 (private side store)
  seats.toml                           (seat list + rotation policy)
  state.json                           (per-seat last_used / cooldown_until / needs_login)
  seats/<name>/auth.json               (per-seat OAuth blob, 0600)
  codex.lock                           (advisory lock; held while codex runs)
```

## Output Format

```
Session: 0199a213-81c0-7800-8aa1-bbab2a035a53

The repository contains three main components...

Tokens: 15228 input (14208 cached), 249 output
```

- **Session ID** is displayed first for easy copying/resuming
- **Stderr is suppressed** on success (no thinking tokens cluttering output)
- **Stderr is shown** on failure to aid debugging
- **Agent messages** are aggregated with newline separators
- **Token usage** is displayed at the end (input, cached, and output tokens)

## How It Works

1. Wraps `codex exec --json --skip-git-repo-check`
2. Captures stdout (JSON events) and stderr (thinking tokens) separately
3. Parses JSON events permissively, extracting:
   - `thread.started` → Session ID
   - `item.completed` with `agent_message` → Final response text
   - `turn.completed` → Token usage stats (input / cached / output / reasoning)
   - `turn.failed` and `error` → Error messages surfaced to stderr
4. Silently ignores other event types (`reasoning`, `command_execution`, `turn.started`, etc.)
5. On success: outputs session ID, aggregated messages, and usage stats; discards stderr
6. On failure: outputs session ID, messages, usage stats, surfaced errors, and codex stderr for debugging
7. Closes child stdin with `Stdio::null()` so codex never waits on an inherited pipe from the parent (prevents hangs when invoked from orchestration tools like Claude Code)

### Generated Commands

| Mode | Command Generated |
|------|-------------------|
| Exec | `codex exec --json --skip-git-repo-check [options] <prompt>` |
| Resume (ID) | `codex exec --json --skip-git-repo-check resume <id> [prompt]` |
| Resume (last) | `codex exec --json --skip-git-repo-check resume --last` (prompt via stdin) |
| Review | `codex exec review --json --skip-git-repo-check [options] [prompt]` |

## CLI Reference

```
codex-clean [OPTIONS...] <prompt>
codex-clean [OPTIONS...] -
codex-clean resume <SESSION_ID> [prompt]
codex-clean resume --last [prompt]
codex-clean review [OPTIONS...] [prompt]
codex-clean seat add <NAME> [--label LABEL] [--import] [--browser]
codex-clean seat list
codex-clean seat login <NAME> [--browser]
codex-clean seat use <NAME>
codex-clean seat remove <NAME> [--yes]
```

| Argument | Description |
|----------|-------------|
| `OPTIONS` | Passed through to `codex exec` (e.g., `-m`, `--sandbox`, `-C`) |
| `prompt` | The prompt to send to codex |
| `-` | Read prompt from stdin |
| `resume` | Resume an existing session |
| `SESSION_ID` | Specific session ID to resume |
| `--last` | Use the most recent session |
| `review` | Review code changes |
| `--uncommitted` | Review uncommitted changes |
| `--base <branch>` | Review changes against a base branch |
| `--commit <sha>` | Review a specific commit |
| `seat add <name>` | Register a new seat. `--import` adopts the existing `~/.codex/auth.json`; otherwise runs `codex login --device-auth` (or `--browser`) in an isolated temp `CODEX_HOME` |
| `seat list` | Table of seats with last-used / cooldown / status |
| `seat login <name>` | Re-authenticate a seat. The new login's `account_id` is verified against the stored value and a mismatch refuses to overwrite |
| `seat use <name>` | Pre-position `~/.codex/auth.json` to this seat's blob and record it as active. Does not disable rotation for subsequent `codex-clean` runs (use `CODEX_CLEAN_SEAT` for that) |
| `seat remove <name>` | Remove a seat (prompts for confirmation unless `--yes`) |

### Environment variables

| Variable | Effect |
|----------|--------|
| `CODEX_CLEAN_SEAT` | Pin a specific seat for this invocation (bypasses rotation; errors if the seat is cooling or `needs_login`) |
| `CODEX_HOME` | Honoured as codex's home directory (default `~/.codex`) — used both as the swap target and by codex itself |
| `CODEX_CLEAN_HOME` | Override the side-store location (default `~/.config/codex-clean`); used by integration tests |

### Exit codes

| Code | Meaning |
|------|---------|
| `0` | Success |
| `1` | Codex error (rate-limit on a pinned seat, auth error, or any other non-zero codex exit) |
| `75` | All seats cooling (`EX_TEMPFAIL`) — try again after the soonest cooldown expiry |

## Features

- **Clean output**: No JSON noise, no thinking tokens on success
- **Session tracking**: Always shows session ID for easy resumption
- **Token usage**: Displays input, cached, and output token counts
- **Code review**: Dedicated `review` subcommand with pass-through flags
- **Multi-seat rotation**: Manages multiple ChatGPT accounts; auto-rotates on rate-limit; cooldowns parsed from codex's own "try again at HH:MM" message
- **Stdin support**: Pipe prompts for scripting workflows
- **Error visibility**: Shows stderr only when codex fails
- **Bounded buffers**: Stderr capped at 10MB to prevent memory issues
- **Safe defaults**: Adds `--json` and `--skip-git-repo-check` automatically; auth files written `0600`, seat dirs `0700` on Unix
- **Prompt validation**: Detects when flags are accidentally used as prompts

## Requirements

- [Codex CLI](https://github.com/openai/codex) v0.124.0+ installed and in PATH (v0.125.0+ recommended for the device-code login flow used by `seat add`)
- Rust 1.70+ (for building from source)

## Licence

MIT
