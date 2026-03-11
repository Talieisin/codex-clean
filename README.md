# codex-clean

A Rust CLI wrapper for `codex exec` that filters JSON output, suppressing stderr (thinking tokens) and extracting only session IDs, final agent messages, and token usage stats.

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
codex-clean -m gpt-5.2-codex --sandbox read-only "explain the main function"

# With config options
codex-clean -m gpt-5.2-codex --config model_reasoning_effort="high" --sandbox read-only "review this code"

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
codex-clean review -m gpt-5.2-codex --uncommitted
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
   - `turn.completed` → Token usage stats
4. Silently ignores other event types (`reasoning`, `command_execution`, `turn.started`, etc.)
5. On success: outputs session ID, aggregated messages, and usage stats; discards stderr
6. On failure: outputs session ID, messages, usage stats, and stderr for debugging

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

## Features

- **Clean output**: No JSON noise, no thinking tokens on success
- **Session tracking**: Always shows session ID for easy resumption
- **Token usage**: Displays input, cached, and output token counts
- **Code review**: Dedicated `review` subcommand with pass-through flags
- **Stdin support**: Pipe prompts for scripting workflows
- **Error visibility**: Shows stderr only when codex fails
- **Bounded buffers**: Stderr capped at 10MB to prevent memory issues
- **Safe defaults**: Adds `--json` and `--skip-git-repo-check` automatically
- **Prompt validation**: Detects when flags are accidentally used as prompts

## Requirements

- [Codex CLI](https://github.com/openai/codex) v0.104.0+ installed and in PATH
- Rust 1.70+ (for building from source)

## Licence

MIT
