//! Live per-seat quota via `codex app-server`.
//!
//! `codex exec --json` never reports quota, so `seat status` spawns codex's
//! app-server (newline-delimited JSON-RPC over stdio) inside an isolated
//! `CODEX_HOME` seeded with one seat's auth blob and calls
//! `account/rateLimits/read`. The snapshot types live in `seat.rs` because
//! they are persisted in `state.json`; this module owns fetching, parsing,
//! and the exhaustion verdict.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Local, Utc};
use serde_json::{json, Value};

use crate::ratelimit::{self, CooldownReason};
use crate::seat::{
    self, RotationConfig, ScratchCodexHome, SeatEntry, SeatIdentity, SeatRuntimeState,
    UsageBucket, UsageCredits, UsageSnapshot, UsageWindow,
};

/// Wall-clock budget for one seat: spawn, handshake, read, teardown.
pub const APP_SERVER_TIMEOUT: Duration = Duration::from_secs(20);
/// Upper bound on concurrent app-server children.
pub const MAX_CONCURRENT_FETCHES: usize = 4;
/// Window sizes we label specially.
pub const FIVE_HOUR_MINUTES: u64 = 300;
pub const WEEKLY_MINUTES: u64 = 10_080;

const STDERR_TAIL_BYTES: usize = 8 * 1024;
/// Largest single JSON-RPC frame we will buffer from the child.
const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// Backlog of unread frames before the reader thread blocks (back-pressure).
const FRAME_CHANNEL_DEPTH: usize = 256;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const RPC_METHOD_NOT_FOUND: i64 = -32601;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a seat's usage could not be fetched. Messages are short reasons only;
/// raw frames and auth material never end up here.
#[derive(Debug)]
pub enum UsageFetchError {
    /// `codex` is not on PATH.
    CodexMissing,
    /// The seat's tokens are not accepted by the app-server.
    AuthRequired,
    /// This codex is too old to know `account/rateLimits/read`.
    MethodNotFound,
    /// Some other JSON-RPC error.
    Rpc(String),
    /// Ran out of time.
    Timeout(Duration),
    /// The child misbehaved (closed early, garbage, missing fields).
    Protocol(String),
    /// Local I/O failure (slot missing, scratch dir, pipes).
    Io(anyhow::Error),
}

impl std::fmt::Display for UsageFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CodexMissing => write!(f, "codex binary not found on PATH"),
            Self::AuthRequired => write!(f, "ChatGPT authentication required"),
            Self::MethodNotFound => write!(
                f,
                "this codex does not support account/rateLimits/read (codex 0.153+ required)"
            ),
            Self::Rpc(m) => write!(f, "app-server error: {}", m),
            Self::Timeout(d) => write!(f, "timed out after {}s", d.as_secs()),
            Self::Protocol(m) => write!(f, "{}", m),
            Self::Io(e) => write!(f, "{:#}", e),
        }
    }
}

impl std::error::Error for UsageFetchError {}

impl From<anyhow::Error> for UsageFetchError {
    fn from(e: anyhow::Error) -> Self {
        Self::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Client seam
// ---------------------------------------------------------------------------

/// Fetches one seat's snapshot. `seat_cmd::status_with` takes a `&dyn
/// UsageClient` so tests can feed canned snapshots without a process.
pub trait UsageClient: Sync {
    fn fetch(&self, seat: &SeatEntry) -> Result<UsageSnapshot, UsageFetchError>;
}

/// Production client: one `codex app-server` child per call.
pub struct AppServerClient {
    pub timeout: Duration,
}

impl Default for AppServerClient {
    fn default() -> Self {
        Self { timeout: APP_SERVER_TIMEOUT }
    }
}

impl UsageClient for AppServerClient {
    fn fetch(&self, seat: &SeatEntry) -> Result<UsageSnapshot, UsageFetchError> {
        let timeout = self.timeout;
        fetch_usage_with(seat, Utc::now(), |home| app_server_rate_limits(home, timeout))
    }
}

/// Fetch every seat, at most [`MAX_CONCURRENT_FETCHES`] at a time. Results
/// come back in the same order as `seats`.
pub fn fetch_all(
    client: &dyn UsageClient,
    seats: &[SeatEntry],
) -> Vec<(String, Result<UsageSnapshot, UsageFetchError>)> {
    let mut out = Vec::with_capacity(seats.len());
    for chunk in seats.chunks(MAX_CONCURRENT_FETCHES) {
        let results: Vec<_> = thread::scope(|s| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|seat| s.spawn(move || (seat.name.clone(), client.fetch(seat))))
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join().unwrap_or_else(|_| {
                        (
                            "?".to_string(),
                            Err(UsageFetchError::Protocol(
                                "fetch thread panicked".to_string(),
                            )),
                        )
                    })
                })
                .collect()
        });
        out.extend(results);
    }
    out
}

/// Core of a fetch, generic over the thing that talks to codex.
///
/// Stages the seat's blob into a scratch `CODEX_HOME`, runs `call`, then —
/// only after the caller has torn the child down — copies any token refresh
/// back into the slot (identity-guarded) and parses the result.
pub fn fetch_usage_with<F>(
    seat: &SeatEntry,
    now: DateTime<Utc>,
    call: F,
) -> Result<UsageSnapshot, UsageFetchError>
where
    F: FnOnce(&Path) -> Result<Value, UsageFetchError>,
{
    let slot = seat::seat_auth_path(&seat.name)?;
    let bytes = fs::read(&slot).with_context(|| {
        format!(
            "seat '{}' has no auth.json at {} (run `codex-clean seat login {}`)",
            seat.name,
            slot.display(),
            seat.name
        )
    })?;
    let expected = complete_identity(seat, &bytes);

    let scratch = ScratchCodexHome::create_for(&seat.name, "status")?;
    seat::atomic_write(&scratch.auth_path(), &bytes)?;
    seat::seed_file_store_config(scratch.path())?;

    let result = call(scratch.path());

    // The app-server may have rotated the refresh token even if the read
    // itself failed; persist whatever it left, but only if it is still this
    // seat's identity.
    match seat::refresh_back_from_guarded(&scratch.auth_path(), &seat.name, &expected) {
        Ok(outcome) => seat::warn_refresh_back(&seat.name, &outcome),
        Err(e) => eprintln!(
            "Warning: failed to persist refreshed token for seat '{}': {:#}",
            seat.name, e
        ),
    }
    drop(scratch);

    let value = result?;
    parse_rate_limits_result(&value, now).map_err(|e| UsageFetchError::Protocol(e.to_string()))
}

/// The seat's configured identity, with gaps filled from its own blob.
fn complete_identity(seat: &SeatEntry, slot_bytes: &[u8]) -> SeatIdentity {
    let mut id = seat.identity();
    if id.account_id.is_none() || id.user_id.is_none() {
        if let Ok(from_blob) = seat::read_identity(slot_bytes) {
            if id.account_id.is_none() {
                id.account_id = from_blob.account_id;
            }
            if id.user_id.is_none() {
                id.user_id = from_blob.user_id;
            }
        }
    }
    id
}

// ---------------------------------------------------------------------------
// app-server child
// ---------------------------------------------------------------------------

/// Kill-on-drop wrapper so every early return reaps the child.
struct ChildGuard(Child);

impl ChildGuard {
    /// Give the child until `grace_until` to exit on its own (stdin is
    /// already closed), then kill it. Returns only once the child has been
    /// reaped; an error means we could not confirm that.
    fn shutdown(&mut self, grace_until: Instant) -> Result<(), String> {
        loop {
            match self.0.try_wait() {
                Ok(Some(_)) => return Ok(()),
                Ok(None) if Instant::now() < grace_until => {
                    thread::sleep(Duration::from_millis(50))
                }
                Ok(None) => break,
                Err(e) => return Err(format!("polling codex app-server: {}", e)),
            }
        }
        if let Err(e) = self.0.kill() {
            // Already gone between try_wait and kill is fine; anything else is not.
            if e.kind() != io::ErrorKind::InvalidInput {
                return Err(format!("killing codex app-server: {}", e));
            }
        }
        self.0
            .wait()
            .map(|_| ())
            .map_err(|e| format!("reaping codex app-server: {}", e))
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Env vars the app-server child is allowed to inherit. Everything else is
/// dropped: the child holds a seat's OAuth blob, so it gets a minimal,
/// documented environment rather than the parent's.
fn env_allowed(key: &OsStr) -> bool {
    const EXACT: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "TMPDIR",
        "TERM",
        "LANG",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
        "all_proxy",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ];
    let Some(k) = key.to_str() else { return false };
    EXACT.contains(&k) || k.starts_with("LC_")
}

/// Spawn `codex app-server` with `CODEX_HOME=home`, perform the handshake,
/// return the `result` of `account/rateLimits/read`. The child is fully torn
/// down (stdin closed, exited or killed, reaped, readers joined) before this
/// returns, so the caller may safely read the scratch auth.json afterwards.
pub fn app_server_rate_limits(home: &Path, timeout: Duration) -> Result<Value, UsageFetchError> {
    let deadline = Instant::now() + timeout;

    let mut cmd = Command::new("codex");
    cmd.arg("app-server");
    cmd.env_clear();
    for (k, v) in std::env::vars_os() {
        if env_allowed(&k) {
            cmd.env(k, v);
        }
    }
    cmd.env("CODEX_HOME", home);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(UsageFetchError::CodexMissing),
        Err(e) => return Err(anyhow::Error::from(e).context("spawning codex app-server").into()),
    };
    let mut guard = ChildGuard(child);
    let mut stdin = guard.0.stdin.take().expect("stdin piped");
    let stdout = guard.0.stdout.take().expect("stdout piped");
    let stderr = guard.0.stderr.take().expect("stderr piped");

    // Reader threads are never joined: if a descendant of the app-server
    // inherited our pipes, a join could block for as long as it lives. The
    // stdout thread exits when the receiver is dropped; the stderr thread
    // hands back its tail through a channel we wait on with a bound.
    let (tx, rx) = mpsc::sync_channel::<io::Result<String>>(FRAME_CHANNEL_DEPTH);
    thread::spawn(move || read_frames(stdout, &tx));
    let (tail_tx, tail_rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        let _ = tail_tx.send(drain_tail(stderr, STDERR_TAIL_BYTES));
    });

    let result = (|| -> Result<Value, UsageFetchError> {
        write_frame(
            &mut stdin,
            &json!({
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "codex-clean",
                        "title": "codex-clean seat status",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            }),
        )?;
        wait_for_response(&rx, 1, deadline)?;
        write_frame(&mut stdin, &json!({"method": "initialized"}))?;
        write_frame(&mut stdin, &json!({"id": 2, "method": "account/rateLimits/read"}))?;
        wait_for_response(&rx, 2, deadline)
    })();

    // Teardown, in order: close stdin, let it exit (within the remaining
    // budget, at most SHUTDOWN_GRACE), kill if it won't, reap. Only after the
    // child is confirmed reaped is the scratch auth.json safe to read.
    drop(stdin);
    let grace_until = Instant::now() + SHUTDOWN_GRACE.min(remaining_or_floor(deadline));
    let teardown = guard.shutdown(grace_until);
    drop(rx);
    let stderr_tail = tail_rx
        .recv_timeout(SHUTDOWN_GRACE.min(remaining_or_floor(deadline)))
        .unwrap_or_default();

    let result = match (result, teardown) {
        (Ok(v), Ok(())) => Ok(v),
        (Ok(_), Err(t)) => Err(UsageFetchError::Protocol(format!(
            "codex app-server answered but could not be shut down cleanly ({}); \
             not trusting the scratch auth state",
            t
        ))),
        (Err(e), _) => Err(e),
    };
    if let Err(UsageFetchError::Protocol(_)) = &result {
        // Diagnostics go to our stderr, never into the error string (which
        // ends up in --json output).
        if let Some(last) = stderr_tail.lines().rev().map(str::trim).find(|l| !l.is_empty()) {
            eprintln!("codex app-server stderr: {}", truncate_chars(last, 300));
        }
    }
    result
}

/// Time left until `deadline`, but never less than a small floor so teardown
/// always gets a real chance even when the request itself timed out.
fn remaining_or_floor(deadline: Instant) -> Duration {
    deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(250))
}

/// Feed newline-delimited frames from the child's stdout into `tx`, bounding
/// each frame to [`MAX_FRAME_BYTES`]. Stops on EOF, on an oversized frame
/// (reported as an error frame), or when the receiver is gone.
fn read_frames(stdout: impl Read, tx: &mpsc::SyncSender<io::Result<String>>) {
    let mut reader = BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    loop {
        buf.clear();
        // read_until with a hard cap: pull bounded chunks so a single
        // unterminated line cannot grow without limit.
        loop {
            let available = match reader.fill_buf() {
                Ok(a) => a,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            };
            if available.is_empty() {
                break; // EOF
            }
            let (chunk, done) = match available.iter().position(|b| *b == b'\n') {
                Some(i) => (&available[..i], true),
                None => (available, false),
            };
            if buf.len() + chunk.len() > MAX_FRAME_BYTES {
                // Report at once: the rest of this frame may never end.
                let _ = tx.send(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("codex app-server emitted a frame larger than {} bytes", MAX_FRAME_BYTES),
                )));
                return;
            }
            buf.extend_from_slice(chunk);
            let consumed = chunk.len() + usize::from(done);
            reader.consume(consumed);
            if done {
                break;
            }
        }
        if buf.is_empty() {
            // EOF with nothing pending.
            return;
        }
        let line = String::from_utf8_lossy(&buf).into_owned();
        if tx.send(Ok(line)).is_err() {
            return;
        }
    }
}

fn write_frame(stdin: &mut impl Write, frame: &Value) -> Result<(), UsageFetchError> {
    let mut line = frame.to_string();
    line.push('\n');
    match stdin.write_all(line.as_bytes()).and_then(|_| stdin.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Err(UsageFetchError::Protocol(
            "codex app-server exited before accepting the request".to_string(),
        )),
        Err(e) => Err(anyhow::Error::from(e).context("writing to codex app-server").into()),
    }
}

/// Read a JSON-RPC response with the given `id` from the line channel.
///
/// Skips notifications and server-to-client requests (anything with a
/// `method`), responses to other ids, and unparseable lines. Accepts the id
/// as a number or a string. Pure over the channel so it is unit-testable.
pub(crate) fn wait_for_response(
    rx: &Receiver<io::Result<String>>,
    id: u64,
    deadline: Instant,
) -> Result<Value, UsageFetchError> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(UsageFetchError::Timeout(remaining));
        }
        let line = match rx.recv_timeout(remaining) {
            Ok(Ok(line)) => line,
            Ok(Err(e)) => {
                return Err(UsageFetchError::Protocol(format!(
                    "reading codex app-server output: {}",
                    e
                )))
            }
            Err(RecvTimeoutError::Timeout) => return Err(UsageFetchError::Timeout(remaining)),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(UsageFetchError::Protocol(
                    "codex app-server closed its output before responding".to_string(),
                ))
            }
        };
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if v.get("method").is_some() {
            continue;
        }
        if !id_matches(v.get("id"), id) {
            continue;
        }
        if let Some(err) = v.get("error") {
            return Err(classify_rpc_error(err));
        }
        if let Some(result) = v.get("result") {
            return Ok(result.clone());
        }
        return Err(UsageFetchError::Protocol(
            "codex app-server response carried neither result nor error".to_string(),
        ));
    }
}

fn id_matches(actual: Option<&Value>, expected: u64) -> bool {
    match actual {
        Some(Value::Number(n)) => n.as_u64() == Some(expected),
        Some(Value::String(s)) => s == &expected.to_string(),
        _ => false,
    }
}

fn classify_rpc_error(err: &Value) -> UsageFetchError {
    let code = err.get("code").and_then(|c| c.as_i64());
    let message = err
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown error");
    if code == Some(RPC_METHOD_NOT_FOUND) {
        return UsageFetchError::MethodNotFound;
    }
    let lower = message.to_lowercase();
    if lower.contains("authentication required") || lower.contains("not logged in") {
        return UsageFetchError::AuthRequired;
    }
    UsageFetchError::Rpc(sanitize_text(message, 200))
}

/// Keep child-controlled text printable and short before it can reach
/// `--json` output: drop control characters, cap the length.
fn sanitize_text(s: &str, max: usize) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    truncate_chars(cleaned.trim(), max)
}

/// Drain a stream to EOF, keeping only the last `cap` bytes.
fn drain_tail(stream: impl Read, cap: usize) -> String {
    let mut reader = BufReader::new(stream);
    let mut tail: Vec<u8> = Vec::with_capacity(cap.min(4096));
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                tail.extend_from_slice(&chunk[..n]);
                if tail.len() > cap {
                    let excess = tail.len() - cap;
                    tail.drain(..excess);
                }
            }
        }
    }
    String::from_utf8_lossy(&tail).into_owned()
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>())
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse the `result` of `account/rateLimits/read` into a snapshot.
///
/// Uses `rateLimitsByLimitId` (one bucket per metered limit) when present,
/// otherwise the single `rateLimits` view. Nulls are skipped; `usedPercent`
/// may be an integer or a float and is clamped to 0..=100.
pub fn parse_rate_limits_result(result: &Value, fetched_at: DateTime<Utc>) -> Result<UsageSnapshot> {
    let rl = result
        .get("rateLimits")
        .filter(|v| v.is_object())
        .ok_or_else(|| anyhow!("response has no rateLimits object"))?;

    let mut buckets = Vec::new();
    match result
        .get("rateLimitsByLimitId")
        .and_then(|v| v.as_object())
        .filter(|m| !m.is_empty())
    {
        Some(map) => {
            for (key, v) in map {
                if v.is_object() {
                    buckets.push(parse_bucket(v, Some(key)));
                }
            }
        }
        None => buckets.push(parse_bucket(rl, None)),
    }

    let credits = rl
        .get("credits")
        .filter(|c| c.is_object())
        .map(|c| UsageCredits {
            has_credits: c.get("hasCredits").and_then(|b| b.as_bool()).unwrap_or(false),
            unlimited: c.get("unlimited").and_then(|b| b.as_bool()).unwrap_or(false),
        });

    Ok(UsageSnapshot {
        fetched_at,
        plan_type: rl.get("planType").and_then(|p| p.as_str()).map(String::from),
        buckets,
        credits,
        spend_control_reached: rl.get("spendControlReached").and_then(|b| b.as_bool()),
    })
}

fn parse_bucket(v: &Value, key: Option<&str>) -> UsageBucket {
    let windows = ["primary", "secondary"]
        .iter()
        .filter_map(|k| v.get(*k))
        .filter_map(parse_window)
        .collect();
    UsageBucket {
        limit_id: v
            .get("limitId")
            .and_then(|s| s.as_str())
            .map(String::from)
            .or_else(|| key.map(String::from)),
        limit_name: v.get("limitName").and_then(|s| s.as_str()).map(String::from),
        windows,
        rate_limit_reached_type: v
            .get("rateLimitReachedType")
            .and_then(|s| s.as_str())
            .map(String::from),
    }
}

fn parse_window(w: &Value) -> Option<UsageWindow> {
    if !w.is_object() {
        return None;
    }
    let used = w.get("usedPercent")?;
    // Floor, never round: 99.5% is not exhausted, and the verdict keys off
    // `>= 100`.
    let used_percent = used
        .as_u64()
        .or_else(|| used.as_f64().map(|f| f.floor().max(0.0) as u64))
        .or_else(|| used.as_i64().map(|i| i.max(0) as u64))?
        .min(100) as u32;
    let window_minutes = w.get("windowDurationMins").and_then(|m| m.as_u64());
    let resets_at = w
        .get("resetsAt")
        .and_then(|t| t.as_i64())
        .and_then(|ts| DateTime::from_timestamp(ts, 0));
    Some(UsageWindow {
        window_minutes,
        used_percent,
        resets_at,
    })
}

// ---------------------------------------------------------------------------
// Verdict + recording
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageVerdict {
    Healthy,
    Exhausted {
        reason: CooldownReason,
        /// Latest reset among exhausted windows; `None` for credits / spend
        /// caps, which do not reset on a schedule.
        resets_at: Option<DateTime<Utc>>,
    },
}

/// Decide whether a snapshot means the seat is unusable right now.
///
/// A window at 100% in any bucket wins (it carries a reset time). Otherwise a
/// backend `spendControlReached` flag counts, or a `rateLimitReachedType` on
/// the **primary** (`codex`) bucket, mapped to a reason by its wording. A
/// reached flag on another bucket (e.g. `premium` credits) only affects that
/// bucket's models, so it is reported by [`apply_snapshot`] as a notice
/// rather than cooling the seat. `credits.hasCredits == false` on its own is
/// *not* exhaustion: Team plans that never bought credits report exactly
/// that while perfectly usable.
pub fn verdict(snap: &UsageSnapshot) -> UsageVerdict {
    if snap.spend_control_reached == Some(true) {
        return UsageVerdict::Exhausted {
            reason: CooldownReason::SpendControl,
            resets_at: None,
        };
    }
    let Some(bucket) = enforcement_bucket(snap) else {
        return UsageVerdict::Healthy;
    };
    let mut any_window = false;
    let mut latest: Option<DateTime<Utc>> = None;
    for w in &bucket.windows {
        if w.used_percent >= 100 {
            any_window = true;
            if let Some(r) = w.resets_at {
                latest = Some(latest.map_or(r, |cur| cur.max(r)));
            }
        }
    }
    if any_window {
        return UsageVerdict::Exhausted {
            reason: CooldownReason::RateLimit,
            resets_at: latest,
        };
    }
    if let Some(kind) = bucket.rate_limit_reached_type.as_deref() {
        return UsageVerdict::Exhausted {
            reason: reason_for_reached_type(kind),
            resets_at: None,
        };
    }
    UsageVerdict::Healthy
}

/// The bucket whose state decides whether the *seat* is usable: the `codex`
/// limit, or the legacy single view (no `limitId`). A snapshot with only
/// model-specific buckets (e.g. just `premium`) enforces nothing — those are
/// reported as notices instead.
pub fn enforcement_bucket(snap: &UsageSnapshot) -> Option<&UsageBucket> {
    snap.buckets
        .iter()
        .find(|b| b.limit_id.as_deref() == Some("codex"))
        .or_else(|| snap.buckets.iter().find(|b| b.limit_id.is_none()))
}

fn reason_for_reached_type(kind: &str) -> CooldownReason {
    if kind.contains("credits") {
        CooldownReason::Credits
    } else {
        CooldownReason::RateLimit
    }
}

/// Record a snapshot into a seat's runtime state and apply the verdict.
///
/// Exhaustion sets (or extends, never shortens) `cooldown_until` with the
/// configured clamp and jitter. A healthy read **never** clears an existing
/// cooldown or `needs_login`: the runner set those from a real failure and
/// a quota read is weaker evidence. Returns human notices for the caller.
pub fn apply_snapshot(
    name: &str,
    entry: &mut SeatRuntimeState,
    snap: UsageSnapshot,
    rotation: &RotationConfig,
    now: DateTime<Utc>,
) -> Vec<String> {
    let mut notices = Vec::new();
    match verdict(&snap) {
        UsageVerdict::Exhausted { reason, resets_at } => {
            let cd = ratelimit::apply_recovery_window(
                resets_at,
                now,
                ratelimit::default_cooldown_for(
                    reason,
                    rotation.default_cooldown_seconds,
                    rotation.cooldown_max_seconds,
                ),
                rotation.cooldown_min_seconds,
                rotation.cooldown_max_seconds,
                rotation.cooldown_jitter_seconds,
            );
            let existing = entry.cooldown_until.filter(|u| *u > now);
            if existing.is_none_or(|u| cd > u) {
                entry.cooldown_until = Some(cd);
                entry.cooldown_reason = Some(reason.as_str().to_string());
                notices.push(format!(
                    "seat '{}' is exhausted ({}); cooling until {}",
                    name,
                    reason,
                    format_local(cd)
                ));
            } else {
                notices.push(format!(
                    "seat '{}' is exhausted ({}); already cooling until {}",
                    name,
                    reason,
                    format_local(existing.unwrap_or(cd))
                ));
            }
        }
        UsageVerdict::Healthy => {
            // Non-enforced buckets (e.g. `premium`) that are exhausted or
            // flagged: warn, do not cool — only that bucket's models are affected.
            let enforced = enforcement_bucket(&snap).map(|b| b as *const UsageBucket);
            for b in &snap.buckets {
                if enforced == Some(b as *const UsageBucket) {
                    continue;
                }
                let id = b.limit_id.as_deref().unwrap_or("?");
                if let Some(kind) = &b.rate_limit_reached_type {
                    notices.push(format!(
                        "seat '{}': the '{}' limit reports {} ({}) — models metered by that limit are \
                         unavailable for this workspace; regular models are unaffected, so the seat is not cooled",
                        name, id, kind, reason_for_reached_type(kind)
                    ));
                }
                for w in b.windows.iter().filter(|w| w.used_percent >= 100) {
                    notices.push(format!(
                        "seat '{}': the '{}' limit's {} window is at 100% ({}) — models metered by that limit are unavailable; the seat is not cooled",
                        name, id, window_label(w.window_minutes), format_resets(w.resets_at, now)
                    ));
                }
            }
            if let Some(u) = entry.cooldown_until.filter(|u| *u > now) {
                notices.push(format!(
                    "seat '{}' is cooling until {} but reports {}; clear with `codex-clean seat status --clear-cooldown {}`",
                    name,
                    format_local(u),
                    summarize_usage_short(&snap),
                    name
                ));
            }
        }
    }
    entry.usage = Some(snap);
    notices
}

// ---------------------------------------------------------------------------
// Presentation helpers (pure)
// ---------------------------------------------------------------------------

/// The bucket to show in the main table: `codex`, else the first.
pub fn primary_bucket(snap: &UsageSnapshot) -> Option<&UsageBucket> {
    snap.buckets
        .iter()
        .find(|b| b.limit_id.as_deref() == Some("codex"))
        .or_else(|| snap.buckets.first())
}

pub fn find_window(bucket: &UsageBucket, minutes: u64) -> Option<&UsageWindow> {
    bucket.windows.iter().find(|w| w.window_minutes == Some(minutes))
}

/// "5h", "weekly", "3d", "12h", "90m", or "?".
pub fn window_label(minutes: Option<u64>) -> String {
    match minutes {
        None => "?".to_string(),
        Some(FIVE_HOUR_MINUTES) => "5h".to_string(),
        Some(WEEKLY_MINUTES) => "weekly".to_string(),
        Some(m) if m % 1440 == 0 => format!("{}d", m / 1440),
        Some(m) if m % 60 == 0 => format!("{}h", m / 60),
        Some(m) => format!("{}m", m),
    }
}

/// Compact label for `seat list`: "5h" / "wk" / derived.
fn window_label_short(minutes: Option<u64>) -> String {
    match minutes {
        Some(WEEKLY_MINUTES) => "wk".to_string(),
        other => window_label(other),
    }
}

/// "2h13m", "45m", "3d2h", "<1m".
pub fn format_duration_short(d: chrono::Duration) -> String {
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        return "<1m".to_string();
    }
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    if days > 0 {
        format!("{}d{}h", days, hours)
    } else if hours > 0 {
        format!("{}h{}m", hours, mins)
    } else {
        format!("{}m", mins)
    }
}

/// "in 2h13m (Tue 09:48)" / "reset" / "-".
pub fn format_resets(resets_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    match resets_at {
        None => "-".to_string(),
        Some(t) if t <= now => "reset".to_string(),
        Some(t) => format!(
            "in {} ({})",
            format_duration_short(t - now),
            t.with_timezone(&Local).format("%a %H:%M")
        ),
    }
}

/// "42% · in 2h13m (Tue 09:48)" for one window.
pub fn format_window_cell(w: &UsageWindow, now: DateTime<Utc>) -> String {
    format!("{}% · {}", w.used_percent, format_resets(w.resets_at, now))
}

/// "5h 42% wk 88%" — offline summary for `seat list`; "-" if no windows.
pub fn summarize_usage_short(snap: &UsageSnapshot) -> String {
    let Some(bucket) = primary_bucket(snap) else { return "-".to_string() };
    if bucket.windows.is_empty() {
        return "-".to_string();
    }
    bucket
        .windows
        .iter()
        .map(|w| format!("{} {}%", window_label_short(w.window_minutes), w.used_percent))
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_local(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%a %H:%M").to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 7, 12, 0, 0).unwrap()
    }

    fn full_response() -> Value {
        json!({
            "rateLimits": {
                "limitId": "codex",
                "limitName": null,
                "planType": "team",
                "primary": {"usedPercent": 42, "windowDurationMins": 300, "resetsAt": now().timestamp() + 8000},
                "secondary": {"usedPercent": 88, "windowDurationMins": 10080, "resetsAt": now().timestamp() + 300000},
                "credits": {"hasCredits": false, "unlimited": false, "balance": null},
                "rateLimitReachedType": null,
                "spendControlReached": false
            },
            "accountId": "acct-1"
        })
    }

    #[test]
    fn parse_full_single_view() {
        let snap = parse_rate_limits_result(&full_response(), now()).unwrap();
        assert_eq!(snap.plan_type.as_deref(), Some("team"));
        assert_eq!(snap.buckets.len(), 1);
        let b = &snap.buckets[0];
        assert_eq!(b.limit_id.as_deref(), Some("codex"));
        assert_eq!(b.windows.len(), 2);
        assert_eq!(b.windows[0].used_percent, 42);
        assert_eq!(b.windows[0].window_minutes, Some(300));
        assert_eq!(b.windows[1].used_percent, 88);
        assert_eq!(b.windows[1].window_minutes, Some(10080));
        assert_eq!(
            snap.credits,
            Some(UsageCredits { has_credits: false, unlimited: false })
        );
        assert_eq!(snap.spend_control_reached, Some(false));
    }

    #[test]
    fn parse_by_limit_id_yields_multiple_buckets() {
        let mut v = full_response();
        v["rateLimitsByLimitId"] = json!({
            "codex": v["rateLimits"].clone(),
            "gpt-5.5-pro": {"primary": {"usedPercent": 10.6, "windowDurationMins": 300}, "secondary": null}
        });
        let snap = parse_rate_limits_result(&v, now()).unwrap();
        assert_eq!(snap.buckets.len(), 2);
        let pro = snap
            .buckets
            .iter()
            .find(|b| b.limit_id.as_deref() == Some("gpt-5.5-pro"))
            .unwrap();
        assert_eq!(pro.windows.len(), 1, "null secondary is skipped");
        assert_eq!(pro.windows[0].used_percent, 10, "float floors, never rounds up");
        assert!(pro.windows[0].resets_at.is_none());
        assert_eq!(primary_bucket(&snap).unwrap().limit_id.as_deref(), Some("codex"));
    }

    #[test]
    fn parse_missing_rate_limits_errors() {
        assert!(parse_rate_limits_result(&json!({"foo": 1}), now()).is_err());
        assert!(parse_rate_limits_result(&json!({"rateLimits": null}), now()).is_err());
    }

    #[test]
    fn parse_never_rounds_a_partial_window_into_exhaustion() {
        let v = json!({"rateLimits": {"primary": {"usedPercent": 99.9, "windowDurationMins": 300}}});
        let snap = parse_rate_limits_result(&v, now()).unwrap();
        assert_eq!(snap.buckets[0].windows[0].used_percent, 99);
        assert_eq!(verdict(&snap), UsageVerdict::Healthy);
        let v = json!({"rateLimits": {"primary": {"usedPercent": 100.0}}});
        let snap = parse_rate_limits_result(&v, now()).unwrap();
        assert!(matches!(verdict(&snap), UsageVerdict::Exhausted { .. }));
    }

    #[test]
    fn rpc_error_text_is_sanitised() {
        let rx = channel_with(&[
            r#"{"id":1,"error":{"code":-32000,"message":"line1\nline2\u001b[31m red"}}"#,
        ]);
        match wait_for_response(&rx, 1, soon()) {
            Err(UsageFetchError::Rpc(m)) => {
                assert!(!m.contains('\n') && !m.contains('\u{1b}'), "{:?}", m);
                assert!(m.starts_with("line1 line2"));
            }
            other => panic!("expected Rpc, got {:?}", other),
        }
    }

    #[test]
    fn read_frames_bounds_frame_size_and_splits_lines() {
        let (tx, rx) = mpsc::sync_channel(16);
        let data = "{\"a\":1}\n{\"b\":2}\n".to_string();
        read_frames(io::Cursor::new(data.into_bytes()), &tx);
        drop(tx);
        let got: Vec<String> = rx.iter().map(|r| r.unwrap()).collect();
        assert_eq!(got, vec!["{\"a\":1}", "{\"b\":2}"]);

        let (tx, rx) = mpsc::sync_channel(16);
        let huge = vec![b'x'; MAX_FRAME_BYTES + 10];
        read_frames(io::Cursor::new(huge), &tx);
        drop(tx);
        let got: Vec<_> = rx.iter().collect();
        assert_eq!(got.len(), 1);
        assert!(got[0].is_err(), "oversized unterminated frame is reported, not buffered");
    }

    #[test]
    fn parse_clamps_out_of_range_percent_and_bad_timestamps() {
        let v = json!({"rateLimits": {
            "primary": {"usedPercent": 250, "resetsAt": i64::MAX},
            "secondary": {"usedPercent": -5}
        }});
        let snap = parse_rate_limits_result(&v, now()).unwrap();
        let w = &snap.buckets[0].windows;
        assert_eq!(w[0].used_percent, 100);
        assert!(w[0].resets_at.is_none(), "unrepresentable timestamp → None");
        assert_eq!(w[1].used_percent, 0);
    }

    fn channel_with(lines: &[&str]) -> Receiver<io::Result<String>> {
        let (tx, rx) = mpsc::channel();
        for l in lines {
            tx.send(Ok(l.to_string())).unwrap();
        }
        rx
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    #[test]
    fn wait_for_response_skips_notifications_requests_and_other_ids() {
        let rx = channel_with(&[
            r#"{"method":"account/rateLimits/updated","params":{}}"#,
            r#"{"id":"srv-1","method":"item/commandExecution/requestApproval","params":{}}"#,
            r#"{"id":7,"result":{"nope":true}}"#,
            "not json",
            r#"{"id":1,"result":{"ok":true}}"#,
        ]);
        let v = wait_for_response(&rx, 1, soon()).unwrap();
        assert_eq!(v, json!({"ok": true}));
    }

    #[test]
    fn wait_for_response_accepts_string_id() {
        let rx = channel_with(&[r#"{"id":"2","result":{"s":1}}"#]);
        assert_eq!(wait_for_response(&rx, 2, soon()).unwrap(), json!({"s": 1}));
    }

    #[test]
    fn wait_for_response_classifies_errors() {
        let rx = channel_with(&[
            r#"{"id":1,"error":{"code":-32600,"message":"chatgpt authentication required to read rate limits"}}"#,
        ]);
        assert!(matches!(
            wait_for_response(&rx, 1, soon()),
            Err(UsageFetchError::AuthRequired)
        ));

        let rx = channel_with(&[r#"{"id":1,"error":{"code":-32601,"message":"Method not found"}}"#]);
        assert!(matches!(
            wait_for_response(&rx, 1, soon()),
            Err(UsageFetchError::MethodNotFound)
        ));

        let rx = channel_with(&[r#"{"id":1,"error":{"code":-32000,"message":"backend exploded"}}"#]);
        match wait_for_response(&rx, 1, soon()) {
            Err(UsageFetchError::Rpc(m)) => assert_eq!(m, "backend exploded"),
            other => panic!("expected Rpc, got {:?}", other),
        }

        let rx = channel_with(&[r#"{"id":1}"#]);
        assert!(matches!(
            wait_for_response(&rx, 1, soon()),
            Err(UsageFetchError::Protocol(_))
        ));
    }

    #[test]
    fn wait_for_response_times_out_and_detects_disconnect() {
        let (tx, rx) = mpsc::channel::<io::Result<String>>();
        let deadline = Instant::now() + Duration::from_millis(50);
        assert!(matches!(
            wait_for_response(&rx, 1, deadline),
            Err(UsageFetchError::Timeout(_))
        ));
        drop(tx);
        assert!(matches!(
            wait_for_response(&rx, 1, soon()),
            Err(UsageFetchError::Protocol(_))
        ));

        let (tx, rx) = mpsc::channel::<io::Result<String>>();
        tx.send(Err(io::Error::new(io::ErrorKind::InvalidData, "bad utf8")))
            .unwrap();
        assert!(matches!(
            wait_for_response(&rx, 1, soon()),
            Err(UsageFetchError::Protocol(_))
        ));
    }

    #[test]
    fn env_allowlist_is_minimal() {
        assert!(env_allowed(OsStr::new("PATH")));
        assert!(env_allowed(OsStr::new("LC_ALL")));
        assert!(env_allowed(OsStr::new("https_proxy")));
        assert!(!env_allowed(OsStr::new("OPENAI_API_KEY")));
        assert!(!env_allowed(OsStr::new("CODEX_HOME")));
        assert!(!env_allowed(OsStr::new("AWS_SECRET_ACCESS_KEY")));
    }

    #[test]
    fn drain_tail_keeps_only_last_bytes() {
        let data = "x".repeat(10_000) + "END";
        let out = drain_tail(io::Cursor::new(data.as_bytes()), 100);
        assert_eq!(out.len(), 100);
        assert!(out.ends_with("END"));
    }

    #[test]
    fn labels_and_formatting() {
        assert_eq!(window_label(Some(300)), "5h");
        assert_eq!(window_label(Some(10080)), "weekly");
        assert_eq!(window_label(Some(2880)), "2d");
        assert_eq!(window_label(Some(120)), "2h");
        assert_eq!(window_label(Some(90)), "90m");
        assert_eq!(window_label(None), "?");
        assert_eq!(format_duration_short(chrono::Duration::seconds(30)), "<1m");
        assert_eq!(format_duration_short(chrono::Duration::seconds(45 * 60)), "45m");
        assert_eq!(format_duration_short(chrono::Duration::seconds(2 * 3600 + 13 * 60)), "2h13m");
        assert_eq!(format_duration_short(chrono::Duration::seconds(3 * 86400 + 2 * 3600)), "3d2h");
        assert_eq!(format_resets(None, now()), "-");
        assert_eq!(format_resets(Some(now() - chrono::Duration::minutes(1)), now()), "reset");
        assert!(format_resets(Some(now() + chrono::Duration::minutes(133)), now()).starts_with("in 2h13m ("));
    }

    fn snap_with(windows: &[(u64, u32, Option<i64>)]) -> UsageSnapshot {
        UsageSnapshot {
            fetched_at: now(),
            plan_type: Some("team".into()),
            buckets: vec![UsageBucket {
                limit_id: Some("codex".into()),
                limit_name: None,
                windows: windows
                    .iter()
                    .map(|(m, p, r)| UsageWindow {
                        window_minutes: Some(*m),
                        used_percent: *p,
                        resets_at: r.map(|s| now() + chrono::Duration::seconds(s)),
                    })
                    .collect(),
                rate_limit_reached_type: None,
            }],
            credits: Some(UsageCredits { has_credits: false, unlimited: false }),
            spend_control_reached: Some(false),
        }
    }

    #[test]
    fn verdict_healthy_even_without_credits() {
        let snap = snap_with(&[(300, 42, Some(8000)), (10080, 88, Some(300000))]);
        assert_eq!(verdict(&snap), UsageVerdict::Healthy);
        assert_eq!(summarize_usage_short(&snap), "5h 42% wk 88%");
    }

    #[test]
    fn verdict_window_exhausted_picks_latest_reset() {
        let snap = snap_with(&[(300, 100, Some(8000)), (10080, 100, Some(300000))]);
        assert_eq!(
            verdict(&snap),
            UsageVerdict::Exhausted {
                reason: CooldownReason::RateLimit,
                resets_at: Some(now() + chrono::Duration::seconds(300000)),
            }
        );
    }

    #[test]
    fn reached_flag_on_secondary_bucket_warns_but_does_not_cool() {
        let mut snap = snap_with(&[(300, 38, Some(8000))]);
        snap.buckets.push(UsageBucket {
            limit_id: Some("premium".into()),
            limit_name: None,
            windows: vec![],
            rate_limit_reached_type: Some("workspace_owner_credits_depleted".into()),
        });
        assert_eq!(verdict(&snap), UsageVerdict::Healthy);
        let mut entry = SeatRuntimeState::default();
        let notices = apply_snapshot("a", &mut entry, snap, &rotation(), now());
        assert!(entry.cooldown_until.is_none());
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("'premium' limit reports workspace_owner_credits_depleted"), "{}", notices[0]);

        // A 100% window on premium is likewise not seat exhaustion.
        let mut snap = snap_with(&[(300, 38, Some(8000))]);
        snap.buckets.push(UsageBucket {
            limit_id: Some("premium".into()),
            limit_name: None,
            windows: vec![UsageWindow { window_minutes: Some(300), used_percent: 100, resets_at: Some(now()) }],
            rate_limit_reached_type: None,
        });
        assert_eq!(verdict(&snap), UsageVerdict::Healthy);
        let notices = apply_snapshot("a", &mut SeatRuntimeState::default(), snap, &rotation(), now());
        assert!(notices[0].contains("'premium' limit's 5h window is at 100%"), "{}", notices[0]);

        // Premium-only snapshot (no codex bucket): nothing is enforced, only warned.
        let snap = UsageSnapshot {
            fetched_at: now(),
            plan_type: Some("team".into()),
            buckets: vec![UsageBucket {
                limit_id: Some("premium".into()),
                limit_name: None,
                windows: vec![],
                rate_limit_reached_type: Some("workspace_owner_credits_depleted".into()),
            }],
            credits: None,
            spend_control_reached: None,
        };
        assert_eq!(verdict(&snap), UsageVerdict::Healthy);
        assert!(enforcement_bucket(&snap).is_none());
        assert_eq!(primary_bucket(&snap).unwrap().limit_id.as_deref(), Some("premium"), "display falls back");

        // Legacy single view (limitId null) is enforced.
        let mut snap = snap_with(&[(300, 100, Some(600))]);
        snap.buckets[0].limit_id = None;
        assert!(matches!(verdict(&snap), UsageVerdict::Exhausted { .. }));
    }

    #[test]
    fn verdict_reached_type_and_spend_control() {
        let mut snap = snap_with(&[(300, 60, Some(8000))]);
        snap.buckets[0].rate_limit_reached_type = Some("workspace_member_credits_depleted".into());
        assert_eq!(
            verdict(&snap),
            UsageVerdict::Exhausted { reason: CooldownReason::Credits, resets_at: None }
        );
        snap.buckets[0].rate_limit_reached_type = Some("workspace_owner_usage_limit_reached".into());
        assert_eq!(
            verdict(&snap),
            UsageVerdict::Exhausted { reason: CooldownReason::RateLimit, resets_at: None }
        );
        snap.buckets[0].rate_limit_reached_type = None;
        snap.spend_control_reached = Some(true);
        assert_eq!(
            verdict(&snap),
            UsageVerdict::Exhausted { reason: CooldownReason::SpendControl, resets_at: None }
        );
    }

    fn rotation() -> RotationConfig {
        RotationConfig {
            cooldown_min_seconds: 60,
            cooldown_max_seconds: 86_400,
            cooldown_jitter_seconds: 0,
            default_cooldown_seconds: 3600,
            ..Default::default()
        }
    }

    #[test]
    fn apply_snapshot_exhausted_sets_cooldown_and_reason() {
        let mut entry = SeatRuntimeState::default();
        let snap = snap_with(&[(300, 100, Some(7200))]);
        let notices = apply_snapshot("a", &mut entry, snap, &rotation(), now());
        assert_eq!(entry.cooldown_until, Some(now() + chrono::Duration::seconds(7200)));
        assert_eq!(entry.cooldown_reason.as_deref(), Some("rate_limit"));
        assert!(entry.usage.is_some());
        assert!(notices[0].contains("exhausted"));
    }

    #[test]
    fn apply_snapshot_credits_uses_default_cooldown_spend_cap_uses_max() {
        let mut entry = SeatRuntimeState::default();
        let mut snap = snap_with(&[(300, 10, Some(7200))]);
        snap.buckets[0].rate_limit_reached_type = Some("workspace_owner_credits_depleted".into());
        apply_snapshot("a", &mut entry, snap, &rotation(), now());
        assert_eq!(entry.cooldown_until, Some(now() + chrono::Duration::seconds(3600)));
        assert_eq!(entry.cooldown_reason.as_deref(), Some("credits"));

        let mut entry = SeatRuntimeState::default();
        let mut snap = snap_with(&[(300, 10, Some(7200))]);
        snap.spend_control_reached = Some(true);
        apply_snapshot("a", &mut entry, snap, &rotation(), now());
        assert_eq!(entry.cooldown_until, Some(now() + chrono::Duration::seconds(86_400)));
        assert_eq!(entry.cooldown_reason.as_deref(), Some("spend_control"));
    }

    #[test]
    fn apply_snapshot_never_shortens_existing_cooldown() {
        let mut entry = SeatRuntimeState::default();
        let longer = now() + chrono::Duration::hours(20);
        entry.cooldown_until = Some(longer);
        entry.cooldown_reason = Some("credits".into());
        let snap = snap_with(&[(300, 100, Some(600))]);
        let notices = apply_snapshot("a", &mut entry, snap, &rotation(), now());
        assert_eq!(entry.cooldown_until, Some(longer));
        assert_eq!(entry.cooldown_reason.as_deref(), Some("credits"));
        assert!(notices[0].contains("already cooling"));
    }

    #[test]
    fn apply_snapshot_healthy_never_clears_state() {
        let mut entry = SeatRuntimeState::default();
        let until = now() + chrono::Duration::hours(1);
        entry.cooldown_until = Some(until);
        entry.needs_login = true;
        let snap = snap_with(&[(300, 5, Some(600))]);
        let notices = apply_snapshot("a", &mut entry, snap, &rotation(), now());
        assert_eq!(entry.cooldown_until, Some(until), "healthy read must not clear a cooldown");
        assert!(entry.needs_login, "healthy read must not clear needs_login");
        assert!(notices[0].contains("--clear-cooldown a"));

        // Expired cooldown: nothing to say.
        let mut entry = SeatRuntimeState {
            cooldown_until: Some(now() - chrono::Duration::hours(1)),
            ..Default::default()
        };
        let notices = apply_snapshot("a", &mut entry, snap_with(&[(300, 5, None)]), &rotation(), now());
        assert!(notices.is_empty());
    }
}
