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

# 4b. Choose how seats are picked
codex-clean seat strategy                 # show the current strategy
codex-clean seat strategy balanced        # keep usage level across seats (see below)
codex-clean seat strategy fixed main      # always prefer 'main'; others only when it is cooling / logged out
codex-clean seat strategy round-robin     # take turns in declaration order
codex-clean seat strategy lru             # back to the default

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

**Strategies.** `rotation.strategy` in `seats.toml` (or `codex-clean seat strategy …`):

| Strategy | Picks | When to use |
|---|---|---|
| `least-recently-used` (default, alias `lru`) | the eligible seat used longest ago | simple alternation; with two healthy seats it behaves like round-robin |
| `round-robin` (`rr`) | the eligible seat after the active one, in declaration order | strict turn-taking |
| `fixed <seat>` | the named seat whenever it is eligible, else the least-recently-used other seat | one primary account with overflow to the others; strict pinning with *no* fallback is `CODEX_CLEAN_SEAT` |
| `balanced` | the eligible seat with the most headroom on its tightest window (lowest used-% across its 5-hour and weekly windows, ties to LRU) | keep usage level across seats so no single seat hits its weekly cap first |

`balanced` reads the usage snapshot `seat status` records in `state.json`. Before picking, any eligible seat whose snapshot is missing or older than `rotation.balance_refresh_seconds` (default 1800) is refreshed through `codex app-server`, so the pick reflects real headroom. A run pays that cost only when a snapshot has gone stale: typically one to three seconds per stale seat, fetched up to four at a time, with a 20-second timeout per seat as the worst case (the lock is held meanwhile). A seat whose tokens the app-server rejects is marked `needs login` rather than picked. A seat with no snapshot counts as unused and gets picked, and the run's own outcome corrects the picture. A seat that is behind stays preferred until it has caught up, which is what "weighted" rotation amounts to.

**How rotation works.** Before each codex invocation, `codex-clean` acquires a per-host advisory lock, picks a seat by the configured strategy, first copies any token refresh the previously active seat received (from plain `codex`, or from the last run) back into that seat's slot, then atomically copies the chosen seat's auth blob into `~/.codex/auth.json`, runs codex, and copies any token refresh codex performed back into the seat's slot. If the run fails with one of codex's exhaustion messages, the seat is cooled and the next eligible seat is tried. Recognised messages and how long the seat cools:

| Message (codex 0.153.x wording) | Recorded reason | Scope | Cooldown |
|---|---|---|---|
| "You've hit your usage limit for <model> …" | `model_limit` | this seat | until the "try again at HH:MM" codex reports, else `default_cooldown_seconds` |
| "You've hit your usage limit …", "Usage limit reached. You've reached your usage limit …" | `rate_limit` | this seat | until the "try again at HH:MM" codex reports, else `default_cooldown_seconds` |
| "Your workspace is out of credits …", "You've reached your workspace credit limit" | `credits` | **every seat in the same workspace** | `default_cooldown_seconds` (top up and carry on) |
| "You hit your spend cap set in your workspace …" | `spend_control` | **every seat in the same workspace** | `cooldown_max_seconds` (admin-set hard stop) |

Codex sometimes delivers these sentences as the *final agent message* rather than an error event (the out-of-credits case does), so on a failed run the last agent message is classified too. Credits and spend caps are workspace-wide (typically the `premium` credit pool for premium models), so seats sharing an `account_id` are cooled together instead of rotating into the same wall. Transient per-minute 429s are left to codex's own retries and are not treated as exhaustion.

**Cooldown rules.** Every cooldown goes through one merge rule. Seeing the same limit again never pushes an existing cooldown later, so repeated `seat status` checks cannot slide it forward. A different reason keeps the later deadline and records the stronger reason (`spend_control` > `model_limit` > `credits` > `rate_limit`), so a hard stop is never hidden behind a weaker one. A fresh usage snapshot counts as the check: once a cooldown expires, a snapshot that still shows the seat exhausted (and no credits) starts a new one without needing a failed run.

**Workspace credits.** On Team and similar plans, usage past a seat's included quota (its 5-hour or weekly window at 100%) is billed to workspace credits once they have been bought. `codex-clean` treats such a seat as *on credits*: usable, but only with your consent. `rotation.credits` in `seats.toml` (or `codex-clean seat credits …`) controls this:

| Mode | In a terminal | In the background (no TTY, e.g. an agent) |
|---|---|---|
| `ask` (default) | prompts: wait, use credits for this run, until quota resets, or always | does not spend; prints the `Seats:` consent warning; exits **77** |
| `never` | no prompt; does not spend | does not spend; exits **77** |
| `always` | spends automatically once every seat's included quota is used up | same |

The prompt looks like this (it runs with the lock released, so other runs are not blocked while you decide):

```
Included quota is used up: main (weekly resets Thu 08:09), backup1 (weekly resets Sun 23:40).
Workspace credits are available. (A run that starts on included quota can still finish on credits.)
  [w] wait for quota (exit 77)   [o] use credits for this run
  [u] use credits until quota resets   [a] always use credits
Choice [w]:
```

Anything other than `o`, `u` or `a` (including just Enter) means wait. Consent can also be given without a prompt: `CODEX_CLEAN_USE_CREDITS=1` (exactly `1`) for one invocation, or `codex-clean seat credits allow` to allow each workspace whose quota is used up until its quota resets (recorded in `state.json`, expires by itself; `seat credits revoke` removes it). Both work in `never` mode too, as explicit consent. Grants are per workspace: allowing one workspace never lets another spend. Seats still inside their included quota are always used before any seat on credits, whatever the strategy; a seat pinned with `CODEX_CLEAN_SEAT` never rotates, and runs on credits only with consent.

Credits that were bought after seats started cooling are picked up automatically. When every seat (or the pinned seat) is cooling for a reason credits can lift, the next run re-checks those seats' usage first (at most once per `rotation.blocked_probe_seconds`, default 300). A seat whose tokens are rejected during that check is marked as needing login.

**What "no credits without consent" can promise.** codex bills credits automatically and reports no quota while a run is in progress, so `codex-clean` can only promise never to *knowingly start* a run on credits, based on the latest usage reading. A run that starts inside the allowance can finish on credits. To narrow that window, a seat with no reading, or with a reading at 80% or more that is older than `blocked_probe_seconds`, is re-checked before a run without consent. A check that fails lets the run proceed with a warning rather than blocking all work.

**Under Claude Code or another agent.** There is no terminal to prompt on, so `ask` behaves like `never`. The run exits **77** (`EX_NOPERM`) and the last stdout line says consent is needed:

```
Seats: included quota used up on main (resets Thu 08:09), backup1 (resets Sun 23:40); workspace credits available but not spent (credits: ask) — needs the user's consent: re-run with CODEX_CLEAN_USE_CREDITS=1 (this run) or run `codex-clean seat credits allow` (until quota resets) — 0 of 2 usable
```

An agent should treat 77 as "ask the user", not as "retry later" (that is 75): relay the message, and on approval re-run with `CODEX_CLEAN_USE_CREDITS=1`, or run `codex-clean seat credits allow` once. `CODEX_CLEAN_NONINTERACTIVE=1` forces this non-interactive behaviour even in a terminal (useful for `$(…)` command substitution, which would otherwise prompt on your terminal).

**Running in the background.** Anything a background caller (an agent, CI, a cron job) needs to act on is put on **stdout**, after the normal output, on every multi-seat run while the pool is degraded:

```
Seats: backup1 needs login (run: codex-clean seat login backup1); main cooling until Mon 04:57, credits — 0 of 2 usable
```

It repeats on every run until fixed, so a caller that only reads stdout cannot miss it. Every significant event is also appended to `~/.config/codex-clean/seat-events.log` (limits hit and what codex said, auth failures, cooldowns and which seats they covered, orphaned blobs, logins, `seat use`) — `codex-clean seat events [--tail N]` prints the recent ones. Both this log and `unmatched.log` are written `0600`, cap every field they record, and roll over once to `<name>.1` at 1 MiB. Unlike `state.json`, the log survives `seat login` and `seat remove`, so "did it ever rotate?" has an answer. If no seat is eligible — at the start of a run, or after rotation has exhausted every seat within one run — the exit status is 75 (`EX_TEMPFAIL`) so callers can branch on it. If every seat needs a login (nothing will recover by waiting) the exit status is 1 instead. With two healthy seats the default LRU strategy alternates between them, which looks the same as round-robin.

**Checking quota (`seat status`).** The table has a `CREDITS` column (`yes`, `yes (<balance>)`, `unlimited`, `none`) and the status shows `quota used; credits not in use (ask)` or `ready (on credits)` for a seat whose included quota is used up; the first notice line states the credits mode and any active grant. `codex exec` never reports quota, so `seat status` asks codex's own app-server instead: for each seat it copies the seat's auth blob into a private scratch `CODEX_HOME` under `seats/<name>.status-<pid>/`, runs `codex app-server` there with a minimal allow-listed environment, calls `account/rateLimits/read` over JSON-RPC, tears the child down, copies any token refresh back into the seat's slot, and deletes the scratch directory. The snapshot (plan, per-window used %, reset times) is recorded in `state.json`, so `seat list` can show it offline. A seat reporting a window at 100% with no credits available (or a backend "limit reached" / spend-cap flag on the main `codex` limit) is marked cooling until its reset time, so the next run skips it even if the text match never fired. With credits available it is marked *on credits* instead, and cooldowns that credits make moot (`rate_limit`, `credits`) are cleared across the workspace; a reading from any seat in a workspace that shows a hard block (spend cap, credits depleted) wins over another seat's reading showing credits. A flag on a secondary limit (such as the `premium` credit pool) is reported as a warning only, because it affects just the models metered by that limit. Apart from the credits case above, a healthy reading never clears an existing cooldown, and it never clears `needs_login`; `--clear-cooldown <name>` does that explicitly. A seat whose tokens the check rejects is marked as needing login. `seat status` refuses to run while another `codex-clean` holds the lock (use `seat list` for the cached snapshot) and keeps `~/.codex/auth.json` in sync with the active seat if the app-server rotated its token. Requires codex 0.153 or newer; a concurrently running plain `codex` session is not supported while `seat status` runs.

```
NAME           LABEL              PLAN     5H                           WEEKLY                       CREDITS        STATUS
main           Main work account  team     0% · in 3h6m (Fri 23:42)     100% · in 3d11h (Tue 08:04)  yes            quota used; credits not in use (ask)
backup1        Backup work accou… team     27% · in 1h16m (Mon 04:40)   4% · in 6d20h (Sun 23:40)    yes            ready

• credits: ask (no active grant)
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
  seats.toml                           (seat list + rotation policy: strategy, fixed_seat, cooldowns)
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
Seat: backup1 (balanced; usage 5h 48% wk 14%, as of 2m ago)
```

(The `Seat:` line appears only when seats are configured.)

- **Session ID** is displayed first for easy copying/resuming
- **`Seat:` line** (multi-seat only) — the last line of the normal output (after `Tokens:` when codex reported usage; a run that failed before reporting usage has no `Tokens:` line, so anchor on the `Seat:` prefix rather than on position). It names the seat that ran, the strategy (or `pinned via CODEX_CLEAN_SEAT`), that seat's last recorded usage and its age, any seats exhausted earlier in the same run, and the outcome if the run failed, e.g. `Seat: backup1 (balanced; usage 5h 48% wk 14%, as of 2m ago)`. Seat names and quota percentages therefore reach any log that captures stdout; set `CODEX_CLEAN_NO_SEAT_LINE=1` to suppress the line
- **`Seats:` trailer** (multi-seat only) — one extra paragraph after `Tokens:` whenever a seat needs login, is cooling, or has used its included quota while credits are available but not allowed (then it states that the user's consent is needed); absent when every seat is usable. Parsers should treat any trailing paragraph beginning `Seats:` as status, not agent output
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
codex-clean seat strategy [NAME [SEAT]]
codex-clean seat credits [ask|never|always|allow|revoke]
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
| `seat strategy [name [seat]]` | Show the rotation strategy, or set it: `least-recently-used`/`lru`, `round-robin`/`rr`, `fixed <seat>`, `balanced` |
| `seat credits [action]` | Show the credits mode, grants and each seat's credit and quota state; set the mode (`ask`, `never`, `always`); `allow` grants every workspace whose quota is used up until its quota resets; `revoke` removes all grants |
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
| `CODEX_CLEAN_USE_CREDITS` | Exactly `1`: consent to spend workspace credits for this invocation (any other value is ignored). Never passed on to codex |
| `CODEX_CLEAN_NONINTERACTIVE` | Exactly `1`: never show the credits prompt, even in a terminal |
| `CODEX_CLEAN_NO_SEAT_LINE` | Any non-empty value suppresses the `Seat:` output line (the `Seats:` warning trailer is never suppressed) |

### Exit codes

| Code | Meaning |
|------|---------|
| `0` | Success |
| `1` | Codex error (rate-limit on a pinned seat, auth error, or any other non-zero codex exit); also when every seat needs a login |
| `75` | All seats cooling (`EX_TEMPFAIL`), whether detected up front or after rotation exhausted every seat within the run — try again after the soonest cooldown expiry |
| `77` | Included quota is used up and only workspace credits remain, without consent (`EX_NOPERM`) — ask the user, then re-run with `CODEX_CLEAN_USE_CREDITS=1` or run `codex-clean seat credits allow` |

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
