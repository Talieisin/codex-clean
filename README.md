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
# (a seat added by login is not made active automatically; `seat use` or the next rotation does that)

# 3. List configured seats and their current state (offline; shows the last recorded usage)
codex-clean seat list

# 3b. Check live quota for every seat (5-hour and weekly windows, plan, reset times)
codex-clean seat status
codex-clean seat status --json          # machine-readable
codex-clean seat status work            # one seat only

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

**How rotation works.** Before each codex invocation, `codex-clean` acquires a per-host advisory lock, picks a seat (LRU or round-robin), first copies any token refresh the previously active seat received (from plain `codex`, or from the last run) back into that seat's slot, then atomically copies the chosen seat's auth blob into `~/.codex/auth.json`, runs codex, and copies any token refresh codex performed back into the seat's slot. If the run fails with one of codex's exhaustion messages, the seat is cooled and the next eligible seat is tried. Recognised messages and how long the seat cools:

| Message (codex 0.153.x wording) | Recorded reason | Scope | Cooldown |
|---|---|---|---|
| "You've hit your usage limit …", "Usage limit reached. You've reached your usage limit …" | `rate_limit` | this seat | until the "try again at HH:MM" codex reports, else `default_cooldown_seconds` |
| "Your workspace is out of credits …", "You've reached your workspace credit limit" | `credits` | **every seat in the same workspace** | `default_cooldown_seconds` (top up and carry on) |
| "You hit your spend cap set in your workspace …" | `spend_control` | **every seat in the same workspace** | `cooldown_max_seconds` (admin-set hard stop) |

Codex sometimes delivers these sentences as the *final agent message* rather than an error event (the out-of-credits case does), so on a failed run the last agent message is classified too. Credits and spend caps are workspace-wide (typically the `premium` credit pool for premium models), so seats sharing an `account_id` are cooled together instead of rotating into the same wall. Transient per-minute 429s are left to codex's own retries and are not treated as exhaustion.

**Running in the background.** Anything a background caller (an agent, CI, a cron job) needs to act on is put on **stdout**, after the normal output, on every multi-seat run while the pool is degraded:

```
Seats: backup1 needs login (run: codex-clean seat login backup1); main cooling until Mon 04:57, credits — 0 of 2 usable
```

It repeats on every run until fixed, so a caller that only reads stdout cannot miss it. Every significant event is also appended to `~/.config/codex-clean/seat-events.log` (limits hit and what codex said, auth failures, cooldowns and which seats they covered, orphaned blobs, logins, `seat use`) — `codex-clean seat events [--tail N]` prints the recent ones. Both this log and `unmatched.log` are written `0600`, cap every field they record, and roll over once to `<name>.1` at 1 MiB. Unlike `state.json`, the log survives `seat login` and `seat remove`, so "did it ever rotate?" has an answer. If no seat is eligible — at the start of a run, or after rotation has exhausted every seat within one run — the exit status is 75 (`EX_TEMPFAIL`) so callers can branch on it. If every seat needs a login (nothing will recover by waiting) the exit status is 1 instead. With two healthy seats the default LRU strategy alternates between them, which looks the same as round-robin.

**Checking quota (`seat status`).** `codex exec` never reports quota, so `seat status` asks codex's own app-server instead: for each seat it copies the seat's auth blob into a private scratch `CODEX_HOME` under `seats/<name>.status-<pid>/`, runs `codex app-server` there with a minimal allow-listed environment, calls `account/rateLimits/read` over JSON-RPC, tears the child down, copies any token refresh back into the seat's slot, and deletes the scratch directory. The snapshot (plan, per-window used %, reset times) is recorded in `state.json`, so `seat list` can show it offline. A seat reporting a window at 100% (or a backend "limit reached" / spend-cap flag on the main `codex` limit) is marked cooling until its reset time, so the next run skips it even if the text match never fired. A flag on a secondary limit (such as the `premium` credit pool) is reported as a warning only, because it affects just the models metered by that limit. A healthy reading never clears an existing cooldown or `needs_login` (those came from a real failure); `--clear-cooldown <name>` does that explicitly. `seat status` refuses to run while another `codex-clean` holds the lock (use `seat list` for the cached snapshot) and keeps `~/.codex/auth.json` in sync with the active seat if the app-server rotated its token. Requires codex 0.153 or newer; a concurrently running plain `codex` session is not supported while `seat status` runs.

```
NAME           LABEL              PLAN     5H                         WEEKLY                     STATUS
main           Main work account  team     12% · in 3h12m (Mon 03:00)  64% · in 2d5h (Wed 09:48)  ready (active)
backup1        Backup work acco…  team     0% · -                      3% · in 6d1h (Sat 01:00)   ready
```

**Safety.** Auth files are written `0600` and seat directories `0700` on Unix; writes are atomic (temp file + rename + parent fsync); concurrent codex-clean invocations serialise via `~/.config/codex-clean/codex.lock`. Every copy of an auth blob into a seat's slot is identity-guarded: the blob's workspace `account_id` *and* user id (`chatgpt_user_id` from the id token) must match the seat's recorded identity. Two seats in the same Team workspace share an `account_id`, so the user id is what stops one colleague's login being filed under another's seat. A blob that fails the guard (or cannot be verified, or is not parseable) is never written to the slot and never destroyed either — it is parked under `~/.config/codex-clean/orphaned/` before anything overwrites it. Re-authenticating the active seat with `seat login` also updates `~/.codex/auth.json`, so the two copies never drift apart. `seat login` applies the same check and refuses to overwrite on a mismatch. Login and status flows run codex against an isolated scratch `CODEX_HOME` so a Ctrl-C never leaves `~/.codex/auth.json` half-replaced; scratch directories older than an hour are swept up on the next `seat add` / `seat login` / `seat status`.

**Backwards compatibility.** With no `seats.toml` present (i.e. you've never run `seat add`), `codex-clean` behaves exactly as before — no auth swaps, no lock, just a passthrough wrapper.

Layout on disc:

```
~/.codex/                              (codex's own home — unchanged)
  auth.json                            (active seat's tokens; swapped before each run)
  config.toml                          (cli_auth_credentials_store = "file" enforced)
  sessions/, state_5.sqlite, ...       (shared across seats)

~/.config/codex-clean/                 (private side store)
  seats.toml                           (seat list + rotation policy)
  state.json                           (per-seat last_used / cooldown_until / cooldown_reason /
                                        needs_login / usage snapshot from `seat status`)
  seats/<name>/auth.json               (per-seat OAuth blob, 0600)
  codex.lock                           (advisory lock; held while codex runs)
  orphaned/auth-<ts>.json              (blobs that failed the identity guard; safe to delete)
  seat-events.log                      (append-only record of limits, auth failures, cooldowns, logins)
  unmatched.log                        (unclassified failures: error events, last agent message, stderr tail)
  seats/<name>.status-<pid>/           (transient CODEX_HOME for `seat status`; auto-removed)
```

## Output Format

```
Session: 0199a213-81c0-7800-8aa1-bbab2a035a53

The repository contains three main components...

Tokens: 15228 input (14208 cached), 249 output
```

- **Session ID** is displayed first for easy copying/resuming
- **`Seats:` trailer** (multi-seat only) — one extra paragraph after `Tokens:` whenever a seat needs login or is cooling; absent when every seat is usable. Parsers should treat any trailing paragraph beginning `Seats:` as status, not agent output
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
codex-clean seat status [NAME] [--json] [--clear-cooldown NAME]
codex-clean seat events [--tail N]
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
| `seat list` | Table of seats with last-used / last recorded usage / status. Offline |
| `seat status [name]` | Query live quota per seat via `codex app-server`, print it, record it in `state.json`, and cool any exhausted seat. `--json` for machine-readable output; `--clear-cooldown <name>` removes a recorded cooldown. Exits 1 if every fetch failed or another codex-clean run holds the lock |
| `seat events [--tail N]` | Print the last N entries (default 20) of `seat-events.log` |
| `seat login <name>` | Re-authenticate a seat. The new login's workspace and user identity are verified against the stored values and a mismatch refuses to overwrite |
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
| `1` | Codex error (rate-limit on a pinned seat, auth error, or any other non-zero codex exit); also when every seat needs a login |
| `75` | All seats cooling (`EX_TEMPFAIL`), whether detected up front or after rotation exhausted every seat within the run — try again after the soonest cooldown expiry |

## Features

- **Clean output**: No JSON noise, no thinking tokens on success
- **Session tracking**: Always shows session ID for easy resumption
- **Token usage**: Displays input, cached, and output token counts
- **Code review**: Dedicated `review` subcommand with pass-through flags
- **Multi-seat rotation**: Manages multiple ChatGPT accounts; auto-rotates on usage-limit, out-of-credits and spend-cap messages; cooldowns parsed from codex's own "try again at HH:MM" message
- **Seat quota**: `seat status` shows each seat's 5-hour and weekly usage live and records it for `seat list`
- **Stdin support**: Pipe prompts for scripting workflows
- **Error visibility**: Shows stderr only when codex fails
- **Bounded buffers**: Stderr capped at 10MB to prevent memory issues
- **Safe defaults**: Adds `--json` and `--skip-git-repo-check` automatically; auth files written `0600`, seat dirs `0700` on Unix
- **Prompt validation**: Detects when flags are accidentally used as prompts

## Requirements

- [Codex CLI](https://github.com/openai/codex) v0.124.0+ installed and in PATH (v0.125.0+ recommended for the device-code login flow used by `seat add`; v0.153.0+ required for `seat status`)
- Rust 1.82+ (for building from source)

## Licence

MIT
