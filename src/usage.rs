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
    self, merge_cooldown, workspace_key, RotationConfig, ScratchCodexHome, SeatConfig, SeatEntry,
    SeatIdentity, SeatRuntimeState, SeatState, UsageBucket, UsageCredits, UsageResets,
    UsageSnapshot, UsageWindow,
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

/// What `account/rateLimitResetCredit/consume` answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetOutcome {
    /// A grant was spent and the eligible windows were reset.
    Reset,
    /// No window currently needs resetting (no grant spent).
    NothingToReset,
    /// The account has no reset grants available.
    NoCredit,
    /// This idempotency key already completed a reset.
    AlreadyRedeemed,
}

impl ResetOutcome {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "reset" => Self::Reset,
            "nothingToReset" => Self::NothingToReset,
            "noCredit" => Self::NoCredit,
            "alreadyRedeemed" => Self::AlreadyRedeemed,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reset => "reset",
            Self::NothingToReset => "nothing_to_reset",
            Self::NoCredit => "no_credit",
            Self::AlreadyRedeemed => "already_redeemed",
        }
    }

    /// Did this spend a grant and change the windows?
    pub fn is_reset(self) -> bool {
        matches!(self, Self::Reset)
    }
}

/// Estimated cost of one session, as codex reports it (micros).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThreadCost {
    pub credits_micros: i64,
    pub usd_micros: Option<i64>,
}

/// Account-wide token activity. There is no credit figure at this level.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountUsage {
    pub lifetime_tokens: Option<i64>,
    pub last_7d_tokens: i64,
}

/// Fetches seat usage and the other per-seat account calls.
/// `seat_cmd::status_with` takes a `&dyn UsageClient` so tests can feed canned
/// answers without a process. The three extra calls have default
/// implementations so a fake only overrides what it exercises.
pub trait UsageClient: Sync {
    fn fetch(&self, seat: &SeatEntry) -> Result<UsageSnapshot, UsageFetchError>;

    /// Redeem a free usage-limit reset for this seat's account.
    fn consume_reset(
        &self,
        _seat: &SeatEntry,
        _credit_id: Option<&str>,
    ) -> Result<ResetOutcome, UsageFetchError> {
        Err(UsageFetchError::Protocol("not supported by this client".to_string()))
    }

    /// Estimated cost of one session.
    fn thread_cost(&self, _seat: &SeatEntry, _thread_id: &str) -> Result<ThreadCost, UsageFetchError> {
        Err(UsageFetchError::Protocol("not supported by this client".to_string()))
    }

    /// Account-wide token activity.
    fn account_usage(&self, _seat: &SeatEntry) -> Result<AccountUsage, UsageFetchError> {
        Err(UsageFetchError::Protocol("not supported by this client".to_string()))
    }

    /// The available reset grants in full (id, title, expiry), for
    /// `seat reset --dry-run`. Ids are never persisted.
    fn list_resets(&self, seat: &SeatEntry) -> Result<ResetListing, UsageFetchError> {
        let _ = seat;
        Err(UsageFetchError::Protocol("not supported by this client".to_string()))
    }
}

/// One redeemable grant, as listed by `seat reset --dry-run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetCredit {
    pub id: String,
    pub title: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// What `--dry-run` found: how many grants the backend reports, and the detail
/// rows when it supplied them (it may report only a count).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResetListing {
    pub available: u32,
    pub credits: Vec<ResetCredit>,
}

/// A client that never fetches (every call fails). Used where automatic
/// usage checks must not spawn codex, e.g. the test-facing entry points.
pub struct NoUsageClient;

impl UsageClient for NoUsageClient {
    fn fetch(&self, _seat: &SeatEntry) -> Result<UsageSnapshot, UsageFetchError> {
        Err(UsageFetchError::Protocol("usage fetching disabled".to_string()))
    }
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

    fn consume_reset(
        &self,
        seat: &SeatEntry,
        credit_id: Option<&str>,
    ) -> Result<ResetOutcome, UsageFetchError> {
        let timeout = self.timeout;
        with_seat_scratch(seat, |home| app_server_consume_reset(home, timeout, credit_id))
    }

    fn thread_cost(&self, seat: &SeatEntry, thread_id: &str) -> Result<ThreadCost, UsageFetchError> {
        let timeout = self.timeout;
        with_seat_scratch(seat, |home| app_server_thread_cost(home, timeout, thread_id))
    }

    fn account_usage(&self, seat: &SeatEntry) -> Result<AccountUsage, UsageFetchError> {
        let timeout = self.timeout;
        with_seat_scratch(seat, |home| app_server_account_usage(home, timeout))
    }

    fn list_resets(&self, seat: &SeatEntry) -> Result<ResetListing, UsageFetchError> {
        let timeout = self.timeout;
        let v = with_seat_scratch(seat, |home| app_server_rate_limits(home, timeout))?;
        let now = Utc::now();
        Ok(ResetListing {
            // The backend may report only a count, with no detail rows.
            available: parse_rate_limits_result(&v, now)
                .ok()
                .and_then(|s| s.resets)
                .map(|r| r.available)
                .unwrap_or(0),
            credits: parse_reset_credits(&v, now),
        })
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
    let value = with_seat_scratch(seat, call)?;
    parse_rate_limits_result(&value, now).map_err(|e| UsageFetchError::Protocol(e.to_string()))
}

/// Stage a seat's auth blob into an isolated scratch `CODEX_HOME`, run `call`
/// against it, then persist any token the child rotated back into the seat's
/// slot (identity-guarded) and remove the scratch home — whether the call
/// succeeded or not. Every app-server call goes through here.
pub fn with_seat_scratch<T, F>(seat: &SeatEntry, call: F) -> Result<T, UsageFetchError>
where
    F: FnOnce(&Path) -> Result<T, UsageFetchError>,
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
    result
}

/// Every available grant in a rate-limits response, for `--dry-run`.
pub fn parse_reset_credits(result: &Value, now: DateTime<Utc>) -> Vec<ResetCredit> {
    result
        .get("rateLimitResetCredits")
        .and_then(|rc| rc.get("credits"))
        .and_then(|c| c.as_array())
        .map(|list| {
            list.iter()
                .filter(|c| c.get("status").and_then(|s| s.as_str()) == Some("available"))
                .filter_map(|c| {
                    let expires_at = c
                        .get("expiresAt")
                        .and_then(|t| t.as_i64())
                        .and_then(|ts| DateTime::from_timestamp(ts, 0));
                    if expires_at.is_some_and(|e| e <= now) {
                        return None;
                    }
                    Some(ResetCredit {
                        id: c.get("id").and_then(|i| i.as_str())?.to_string(),
                        title: c.get("title").and_then(|t| t.as_str()).map(|t| sanitize_text(t, 60)),
                        expires_at,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_thread_cost(v: &Value) -> Option<ThreadCost> {
    let t = v.get("threadUsage").unwrap_or(v);
    Some(ThreadCost {
        credits_micros: t.get("estimatedUsageCreditsMicros").and_then(|n| n.as_i64())?,
        usd_micros: t.get("estimatedUsageUsdMicros").and_then(|n| n.as_i64()),
    })
}

fn parse_account_usage(v: &Value) -> AccountUsage {
    let lifetime_tokens = v
        .pointer("/summary/lifetimeTokens")
        .and_then(|n| n.as_i64());
    // Daily buckets are ISO dates; sum the last 7 entries the backend sent.
    let mut buckets: Vec<(String, i64)> = v
        .get("dailyUsageBuckets")
        .and_then(|b| b.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|b| {
                    Some((
                        b.get("startDate").and_then(|d| d.as_str())?.to_string(),
                        b.get("tokens").and_then(|t| t.as_i64()).unwrap_or(0),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    buckets.sort_by(|a, b| a.0.cmp(&b.0));
    let last_7d_tokens = buckets.iter().rev().take(7).map(|(_, t)| *t).sum();
    AccountUsage { lifetime_tokens, last_7d_tokens }
}

/// "≈0.42 credits (≈$0.05)", or credits alone when no dollar figure is given.
pub fn format_cost(cost: &ThreadCost) -> String {
    let credits = cost.credits_micros as f64 / 1_000_000.0;
    match cost.usd_micros {
        Some(usd) => format!("≈{:.2} credits (≈${:.2})", credits, usd as f64 / 1_000_000.0),
        None => format!("≈{:.2} credits", credits),
    }
}

/// "1.33B" / "46.3M" / "9,120" for token counts.
pub fn format_tokens(n: i64) -> String {
    match n {
        n if n >= 1_000_000_000 => format!("{:.2}B", n as f64 / 1e9),
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
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
    Ok(app_server_calls(home, timeout, &[("account/rateLimits/read", Value::Null)])?
        .pop()
        .expect("one call, one answer"))
}

/// Redeem a free usage-limit reset. `credit_id` picks a specific grant; the
/// backend chooses the next available one when it is `None`.
pub fn app_server_consume_reset(
    home: &Path,
    timeout: Duration,
    credit_id: Option<&str>,
) -> Result<ResetOutcome, UsageFetchError> {
    // One logical attempt = one key. We never retry a consume automatically,
    // so a fresh key per call is right.
    let key = format!(
        "codex-clean-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let mut params = json!({ "idempotencyKey": key });
    if let Some(id) = credit_id {
        params["creditId"] = json!(id);
    }
    let v = app_server_calls(home, timeout, &[("account/rateLimitResetCredit/consume", params)])?
        .pop()
        .expect("one call, one answer");
    ResetOutcome::parse(v.get("outcome").and_then(|o| o.as_str()).unwrap_or(""))
        .ok_or_else(|| UsageFetchError::Protocol("unrecognised reset outcome".to_string()))
}

/// Estimated cost of one session (thread).
pub fn app_server_thread_cost(
    home: &Path,
    timeout: Duration,
    thread_id: &str,
) -> Result<ThreadCost, UsageFetchError> {
    let v = app_server_calls(
        home,
        timeout,
        &[("account/usage/read", json!({ "threadId": thread_id }))],
    )?
    .pop()
    .expect("one call, one answer");
    parse_thread_cost(&v).ok_or_else(|| {
        UsageFetchError::Protocol("codex reported no usage for that session".to_string())
    })
}

/// Account-wide token usage (no credit figure exists at this level).
pub fn app_server_account_usage(
    home: &Path,
    timeout: Duration,
) -> Result<AccountUsage, UsageFetchError> {
    let v = app_server_calls(home, timeout, &[("account/usage/read", Value::Null)])?
        .pop()
        .expect("one call, one answer");
    Ok(parse_account_usage(&v))
}

/// Spawn one `codex app-server`, handshake, issue `calls` in order, and tear
/// the child down completely before returning. Results are in call order.
pub fn app_server_calls(
    home: &Path,
    timeout: Duration,
    calls: &[(&str, Value)],
) -> Result<Vec<Value>, UsageFetchError> {
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

    let result = (|| -> Result<Vec<Value>, UsageFetchError> {
        write_frame(
            &mut stdin,
            &json!({
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "codex-clean",
                        "title": "codex-clean",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            }),
        )?;
        wait_for_response(&rx, 1, deadline)?;
        write_frame(&mut stdin, &json!({"method": "initialized"}))?;
        let mut out = Vec::with_capacity(calls.len());
        for (i, (method, params)) in calls.iter().enumerate() {
            let id = i as u64 + 2;
            let mut frame = json!({ "id": id, "method": method });
            if !params.is_null() {
                frame["params"] = params.clone();
            }
            write_frame(&mut stdin, &frame)?;
            out.push(wait_for_response(&rx, id, deadline)?);
        }
        Ok(out)
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

    // Report the caller's full budget on timeout, not the sliver that was
    // left when the deadline hit.
    let result = result.map_err(|e| match e {
        UsageFetchError::Timeout(_) => UsageFetchError::Timeout(timeout),
        other => other,
    });
    let result = match (result, teardown) {
        (Ok(v), Ok(())) => Ok(v),
        #[allow(unreachable_patterns)]
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
    if lower.contains("authentication required")
        || lower.contains("not logged in")
        || ratelimit::is_auth_error(message)
    {
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
            balance: match c.get("balance") {
                Some(Value::String(b)) if !b.trim().is_empty() => Some(sanitize_text(b, 32)),
                Some(Value::Number(n)) => Some(n.to_string()),
                _ => None,
            },
        });

    let resets = result
        .get("rateLimitResetCredits")
        .filter(|v| v.is_object())
        .map(|rc| {
            // Only entries the backend still calls `available` can be redeemed;
            // report the earliest future expiry among them.
            let mut next: Option<(DateTime<Utc>, Option<String>)> = None;
            for c in rc.get("credits").and_then(|c| c.as_array()).into_iter().flatten() {
                if c.get("status").and_then(|s| s.as_str()) != Some("available") {
                    continue;
                }
                let Some(exp) = c.get("expiresAt").and_then(|t| t.as_i64()).and_then(|ts| DateTime::from_timestamp(ts, 0)) else {
                    continue;
                };
                if exp <= fetched_at {
                    continue;
                }
                let title = c.get("title").and_then(|t| t.as_str()).map(|t| sanitize_text(t, 60));
                if next.as_ref().is_none_or(|(cur, _)| exp < *cur) {
                    next = Some((exp, title));
                }
            }
            UsageResets {
                available: rc
                    .get("availableCount")
                    .and_then(|n| n.as_u64())
                    .unwrap_or(0)
                    .min(u32::MAX as u64) as u32,
                next_expires_at: next.as_ref().map(|(e, _)| *e),
                next_title: next.and_then(|(_, t)| t),
            }
        });

    Ok(UsageSnapshot {
        resets,
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
    /// Included quota is used up (an enforced window is at 100% and has not
    /// reset yet) but the workspace has credits or is unlimited: runs would
    /// be billed to credits. Not a cooldown — whether such a seat may run is
    /// decided by the credit policy when picking.
    OnCredits {
        /// When the exhausted window(s) reset (the latest of them).
        resets_at: Option<DateTime<Utc>>,
    },
    Exhausted {
        reason: CooldownReason,
        /// Latest reset among exhausted windows; `None` for credits / spend
        /// caps, which do not reset on a schedule.
        resets_at: Option<DateTime<Utc>>,
    },
}

/// True when the workspace can pay for usage past the included quota.
pub fn credits_available(snap: &UsageSnapshot) -> bool {
    snap.credits
        .as_ref()
        .is_some_and(|c| c.has_credits || c.unlimited)
}

/// Decide what a snapshot means for the seat at `now`.
///
/// 1. `spendControlReached` → `Exhausted(SpendControl)`.
/// 2. A `rateLimitReachedType` on the enforced (`codex`) bucket → `Exhausted`,
///    mapped by wording. The backend's own flag is authoritative, so it is
///    checked before the windows.
/// 3. An enforced window at 100% whose reset is still ahead (or unknown) →
///    `OnCredits` when credits are available, else `Exhausted(RateLimit)`.
///    A window whose reset time has passed is treated as reset, so a stale
///    100% never blocks.
/// 4. Otherwise `Healthy`.
///
/// Flags and windows on other buckets (e.g. `premium`) only affect those
/// models and are reported as notices by [`reconcile_snapshots`].
/// `credits.hasCredits == false` on its own is *not* exhaustion.
pub fn verdict(snap: &UsageSnapshot, now: DateTime<Utc>) -> UsageVerdict {
    if snap.spend_control_reached == Some(true) {
        return UsageVerdict::Exhausted {
            reason: CooldownReason::SpendControl,
            resets_at: None,
        };
    }
    let Some(bucket) = enforcement_bucket(snap) else {
        return UsageVerdict::Healthy;
    };
    if let Some(kind) = bucket.rate_limit_reached_type.as_deref() {
        return UsageVerdict::Exhausted {
            reason: reason_for_reached_type(kind),
            resets_at: None,
        };
    }
    let mut any_window = false;
    let mut latest: Option<DateTime<Utc>> = None;
    for w in &bucket.windows {
        if w.used_percent >= 100 && w.resets_at.is_none_or(|r| r > now) {
            any_window = true;
            if let Some(r) = w.resets_at {
                latest = Some(latest.map_or(r, |cur| cur.max(r)));
            }
        }
    }
    if any_window {
        if credits_available(snap) {
            return UsageVerdict::OnCredits { resets_at: latest };
        }
        return UsageVerdict::Exhausted {
            reason: CooldownReason::RateLimit,
            resets_at: latest,
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

/// Map a backend `rateLimitReachedType` to a cooldown reason.
///
/// - `workspace_*_credits_depleted` → `Credits` (workspace-wide, lifted by
///   buying credits);
/// - `workspace_owner/member_usage_limit_reached` → `SpendControl`: an
///   admin-set workspace limit ("increase your limits"), workspace-wide and
///   not lifted by credits;
/// - anything else (`rate_limit_reached`, unknown) → `RateLimit` (personal).
fn reason_for_reached_type(kind: &str) -> CooldownReason {
    if kind.contains("credits") {
        CooldownReason::Credits
    } else if kind.starts_with("workspace_") && kind.contains("usage_limit") {
        CooldownReason::SpendControl
    } else {
        CooldownReason::RateLimit
    }
}

/// Where a seat stands on its included quota, from its recorded snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaState {
    /// Inside the included quota (or exhausted without credits — that case is
    /// represented by a cooldown, which the picker checks separately).
    Within,
    /// Included quota used up; only workspace credits remain.
    OnCredits { resets_at: Option<DateTime<Utc>> },
    /// No snapshot recorded yet.
    Unknown,
}

impl QuotaState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Within => "within",
            Self::OnCredits { .. } => "on_credits",
            Self::Unknown => "unknown",
        }
    }
}

pub fn quota_state(st: &SeatRuntimeState, now: DateTime<Utc>) -> QuotaState {
    match &st.usage {
        None => QuotaState::Unknown,
        Some(snap) => match verdict(snap, now) {
            UsageVerdict::OnCredits { resets_at } => QuotaState::OnCredits { resets_at },
            _ => QuotaState::Within,
        },
    }
}

fn cooldown_until_for(
    reason: CooldownReason,
    resets_at: Option<DateTime<Utc>>,
    rotation: &RotationConfig,
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    ratelimit::apply_recovery_window(
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
    )
}

/// A successful reset whose follow-up reading failed: the recorded snapshot
/// is now known to be wrong (it still says exhausted), so drop it and lift the
/// window cooldown the reset removed. Login, credits and spend-cap blockers
/// stay. Without this the grant would be spent and the seat still blocked.
pub fn invalidate_after_reset(state: &mut SeatState, seat: &str, now: DateTime<Utc>) -> bool {
    let st = state.get(seat);
    let reason = CooldownReason::parse(st.cooldown_reason.as_deref().unwrap_or(""));
    let entry = state.entry_mut(seat);
    entry.usage = None;
    if entry.cooldown_until.is_some_and(|u| u > now) && reason.is_window_based() {
        entry.cooldown_until = None;
        entry.cooldown_reason = None;
        return true;
    }
    false
}

/// After a successful free reset, drop the cooldown the reset just lifted.
///
/// `reconcile_snapshots` only clears cooldowns on credit evidence, so without
/// this the seat would stay cooling and the grant would be wasted. Only
/// window-based reasons are cleared, and only when the refreshed reading
/// agrees the seat is usable; `credits`, `spend_control` and `needs_login`
/// are left alone.
pub fn clear_window_cooldown_after_reset(
    state: &mut SeatState,
    seat: &str,
    now: DateTime<Utc>,
) -> bool {
    let st = state.get(seat);
    let Some(snap) = st.usage.as_ref() else { return false };
    if matches!(verdict(snap, now), UsageVerdict::Exhausted { .. }) {
        return false;
    }
    let reason = CooldownReason::parse(st.cooldown_reason.as_deref().unwrap_or(""));
    if st.cooldown_until.is_none_or(|u| u <= now) || !reason.is_window_based() {
        return false;
    }
    let entry = state.entry_mut(seat);
    entry.cooldown_until = None;
    entry.cooldown_reason = None;
    true
}

/// Re-establish cooldowns from **cached** readings: a seat with no active
/// cooldown whose recorded snapshot still shows an enforced window at 100%
/// (reset still ahead) with no credits is exhausted, whatever the cooldown
/// clock says. Without this, a seat whose clamped cooldown expired just after
/// a fresh "still exhausted" reading would run immediately. Returns the seats
/// re-cooled. (A stale reading on a cooling seat is later re-checked by the
/// blocked-run probe, which is how purchased credits are still discovered.)
pub fn reapply_cached_exhaustion(
    cfg: &SeatConfig,
    state: &mut SeatState,
    now: DateTime<Utc>,
) -> Vec<String> {
    let mut recooled = Vec::new();
    for s in &cfg.seats {
        let st = state.get(&s.name);
        if st.needs_login || st.cooldown_until.is_some_and(|u| u > now) {
            continue;
        }
        let Some(snap) = st.usage.as_ref() else { continue };
        if let UsageVerdict::Exhausted { reason, resets_at: Some(r) } = verdict(snap, now) {
            if reason.is_window_based() && r > now {
                let until = cooldown_until_for(reason, Some(r), &cfg.rotation, now);
                if merge_cooldown(state.entry_mut(&s.name), until, reason, now) {
                    recooled.push(s.name.clone());
                }
            }
        }
    }
    recooled
}

/// Record a batch of freshly fetched snapshots and bring cooldowns in line
/// with them. The one place snapshot evidence changes seat state (used by
/// `seat status`, the balanced refresh, and the blocked-run probe).
///
/// 1. Every snapshot is recorded.
/// 2. A seat's own personal exhaustion (a window at 100% with no credits, a
///    per-seat reached flag) cools that seat, through `merge_cooldown`.
/// 3. Per workspace, over the fetched members: a workspace-wide blocker
///    (spend cap, or a credits-depleted flag) cools every member of the
///    workspace — **blockers dominate regardless of order**; otherwise, if any
///    member shows credits available, cooldowns that credits make moot
///    (`rate_limit`, `credits`) are cleared on every member. Clearing never
///    spends anything: the credit policy still gates `OnCredits` seats.
/// 4. `needs_login` is never touched.
///
/// Returns `(seat, notice)` pairs for display and the events log.
pub fn reconcile_snapshots(
    cfg: &SeatConfig,
    state: &mut SeatState,
    fetched: Vec<(String, UsageSnapshot)>,
    now: DateTime<Utc>,
) -> Vec<(String, String)> {
    let mut notices: Vec<(String, String)> = Vec::new();
    let verdicts: Vec<(String, UsageVerdict)> = fetched
        .iter()
        .map(|(n, snap)| (n.clone(), verdict(snap, now)))
        .collect();

    for (name, snap) in &fetched {
        // Model-specific buckets: warn only.
        let enforced = enforcement_bucket(snap).map(|b| b as *const UsageBucket);
        for b in &snap.buckets {
            if enforced == Some(b as *const UsageBucket) {
                continue;
            }
            let id = b.limit_id.as_deref().unwrap_or("?");
            if let Some(kind) = &b.rate_limit_reached_type {
                notices.push((
                    name.clone(),
                    format!(
                        "seat '{}': the '{}' limit reports {} ({}) — models metered by that limit are \
                         unavailable for this workspace; regular models are unaffected, so the seat is not cooled",
                        name, id, kind, reason_for_reached_type(kind)
                    ),
                ));
            }
            for w in b.windows.iter().filter(|w| w.used_percent >= 100) {
                notices.push((
                    name.clone(),
                    format!(
                        "seat '{}': the '{}' limit's {} window is at 100% ({}) — models metered by that limit are unavailable; the seat is not cooled",
                        name, id, window_label(w.window_minutes), format_resets(w.resets_at, now)
                    ),
                ));
            }
        }
        state.entry_mut(name).usage = Some(snap.clone());
    }

    // Personal exhaustion cools just that seat.
    for (name, v) in &verdicts {
        if let UsageVerdict::Exhausted { reason, resets_at } = v {
            if reason.is_window_based() {
                let until = cooldown_until_for(*reason, *resets_at, &cfg.rotation, now);
                if merge_cooldown(state.entry_mut(name), until, *reason, now) {
                    notices.push((
                        name.clone(),
                        format!("seat '{}' is exhausted ({}); cooling until {}", name, reason, format_local(until)),
                    ));
                }
            }
        }
    }

    // Workspace-level reconciliation.
    let mut workspaces: std::collections::BTreeMap<String, Vec<usize>> = std::collections::BTreeMap::new();
    for (i, (name, _)) in fetched.iter().enumerate() {
        workspaces.entry(workspace_key(cfg, name)).or_default().push(i);
    }
    for idxs in workspaces.values() {
        let first = &fetched[idxs[0]].0;
        let members = seat::workspace_siblings(cfg, first);
        // Strongest workspace-wide blocker among the fetched members.
        let blocker = idxs
            .iter()
            .filter_map(|&i| match verdicts[i].1 {
                UsageVerdict::Exhausted { reason, resets_at } if !reason.is_window_based() => {
                    Some((reason, resets_at))
                }
                _ => None,
            })
            .max_by_key(|(r, _)| r.strength());
        if let Some((reason, resets_at)) = blocker {
            let until = cooldown_until_for(reason, resets_at, &cfg.rotation, now);
            let changed = seat::cool_seats(state, &members, until, reason, now);
            if !changed.is_empty() {
                notices.push((
                    first.clone(),
                    format!(
                        "{} is workspace-wide; cooling {} until {}",
                        reason,
                        changed.join(", "),
                        format_local(until)
                    ),
                ));
            }
            continue;
        }
        let fresh_credits = idxs
            .iter()
            .find(|&&i| credits_available(&fetched[i].1))
            .and_then(|&i| fetched[i].1.credits.clone());
        let Some(fresh_credits) = fresh_credits else {
            continue;
        };
        for m in &members {
            // Credits are workspace-wide: carry the fresh credit reading into
            // members that were not fetched, so a sibling whose cached reading
            // says "100%, no credits" becomes `OnCredits` (gated by consent)
            // instead of looking in-quota once its cooldown is cleared.
            if !fetched.iter().any(|(n, _)| n == m) {
                if let Some(u) = state.entry_mut(m).usage.as_mut() {
                    u.credits = Some(fresh_credits.clone());
                }
            }
            // Guard: a member whose own fresh snapshot says it is exhausted
            // without credits keeps its cooldown (cannot normally happen, as
            // credits are workspace-wide).
            let own_exhausted = fetched
                .iter()
                .zip(&verdicts)
                .any(|((n, _), (_, v))| n == m && matches!(v, UsageVerdict::Exhausted { .. }));
            if own_exhausted {
                continue;
            }
            let entry = state.entry_mut(m);
            let active = entry.cooldown_until.filter(|u| *u > now);
            let reason = CooldownReason::parse(entry.cooldown_reason.as_deref().unwrap_or(""));
            if active.is_some() && reason.is_clearable_by_credits() {
                entry.cooldown_until = None;
                entry.cooldown_reason = None;
                notices.push((
                    m.clone(),
                    format!("seat '{}': workspace credits available; cleared its {} cooldown", m, reason),
                ));
            }
        }
    }

    // Anything still cooling while its fresh snapshot looks usable.
    for (name, v) in &verdicts {
        if matches!(v, UsageVerdict::Exhausted { .. }) {
            continue;
        }
        let st = state.get(name);
        if let Some(u) = st.cooldown_until.filter(|u| *u > now) {
            notices.push((
                name.clone(),
                format!(
                    "seat '{}' is cooling until {} ({}) but reports {}; clear with `codex-clean seat status --clear-cooldown {}`",
                    name,
                    format_local(u),
                    st.cooldown_reason.as_deref().unwrap_or("rate_limit"),
                    st.usage.as_ref().map(summarize_usage_short).unwrap_or_else(|| "-".into()),
                    name
                ),
            ));
        }
    }
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

/// Credits cell: `yes`, `yes (<balance>)`, `unlimited`, `none`, or `-`.
pub fn format_credits(snap: &UsageSnapshot) -> String {
    match &snap.credits {
        None => "-".to_string(),
        Some(c) if c.unlimited => "unlimited".to_string(),
        Some(c) if c.has_credits => match &c.balance {
            Some(b) => format!("yes ({})", b),
            None => "yes".to_string(),
        },
        Some(_) => "none".to_string(),
    }
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
            Some(UsageCredits { has_credits: false, unlimited: false, balance: None })
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
        assert_eq!(verdict(&snap, now()), UsageVerdict::Healthy);
        let v = json!({"rateLimits": {"primary": {"usedPercent": 100.0}}});
        let snap = parse_rate_limits_result(&v, now()).unwrap();
        assert!(matches!(verdict(&snap, now()), UsageVerdict::Exhausted { .. }));
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
            credits: Some(UsageCredits { has_credits: false, unlimited: false, balance: None }),
            spend_control_reached: Some(false),
            resets: None,
        }
    }

    fn with_credits(mut snap: UsageSnapshot) -> UsageSnapshot {
        snap.credits = Some(UsageCredits { has_credits: true, unlimited: false, balance: Some("42".into()) });
        snap
    }

    #[test]
    fn verdict_healthy_even_without_credits() {
        let snap = snap_with(&[(300, 42, Some(8000)), (10080, 88, Some(300000))]);
        assert_eq!(verdict(&snap, now()), UsageVerdict::Healthy);
        assert_eq!(summarize_usage_short(&snap), "5h 42% wk 88%");
    }

    #[test]
    fn verdict_window_exhausted_picks_latest_reset() {
        let snap = snap_with(&[(300, 100, Some(8000)), (10080, 100, Some(300000))]);
        assert_eq!(
            verdict(&snap, now()),
            UsageVerdict::Exhausted {
                reason: CooldownReason::RateLimit,
                resets_at: Some(now() + chrono::Duration::seconds(300000)),
            }
        );
    }

    #[test]
    fn verdict_is_credits_aware() {
        let snap = with_credits(snap_with(&[(300, 0, Some(8000)), (10080, 100, Some(300000))]));
        assert_eq!(
            verdict(&snap, now()),
            UsageVerdict::OnCredits { resets_at: Some(now() + chrono::Duration::seconds(300000)) }
        );
        let mut unlimited = snap_with(&[(10080, 100, Some(60))]);
        unlimited.credits = Some(UsageCredits { has_credits: false, unlimited: true, balance: None });
        assert!(matches!(verdict(&unlimited, now()), UsageVerdict::OnCredits { .. }));
        // Missing credits info counts as no credits.
        let mut none = snap_with(&[(10080, 100, Some(60))]);
        none.credits = None;
        assert!(matches!(verdict(&none, now()), UsageVerdict::Exhausted { .. }));
        // The backend's reached flag wins over credits.
        let mut flagged = snap.clone();
        flagged.buckets[0].rate_limit_reached_type = Some("rate_limit_reached".into());
        assert!(matches!(verdict(&flagged, now()), UsageVerdict::Exhausted { reason: CooldownReason::RateLimit, .. }));
        // Spend cap wins over credits.
        let mut capped = snap.clone();
        capped.spend_control_reached = Some(true);
        assert!(matches!(verdict(&capped, now()), UsageVerdict::Exhausted { reason: CooldownReason::SpendControl, .. }));
        // A 100% window whose reset has passed is treated as reset.
        let stale = snap_with(&[(10080, 100, Some(-60))]);
        assert_eq!(verdict(&stale, now()), UsageVerdict::Healthy);
    }

    #[test]
    fn quota_state_and_credits_formatting() {
        let mut st = SeatRuntimeState::default();
        assert_eq!(quota_state(&st, now()), QuotaState::Unknown);
        st.usage = Some(snap_with(&[(10080, 40, Some(60))]));
        assert_eq!(quota_state(&st, now()), QuotaState::Within);
        st.usage = Some(with_credits(snap_with(&[(10080, 100, Some(60))])));
        assert!(matches!(quota_state(&st, now()), QuotaState::OnCredits { .. }));
        assert_eq!(QuotaState::Unknown.as_str(), "unknown");

        let mut s = snap_with(&[(300, 1, None)]);
        assert_eq!(format_credits(&s), "none");
        s.credits = Some(UsageCredits { has_credits: true, unlimited: false, balance: None });
        assert_eq!(format_credits(&s), "yes");
        s.credits = Some(UsageCredits { has_credits: true, unlimited: false, balance: Some("12.50".into()) });
        assert_eq!(format_credits(&s), "yes (12.50)");
        s.credits = Some(UsageCredits { has_credits: false, unlimited: true, balance: None });
        assert_eq!(format_credits(&s), "unlimited");
        s.credits = None;
        assert_eq!(format_credits(&s), "-");
    }

    #[test]
    fn parse_reads_credit_balance_as_string_or_number() {
        for (raw, expected) in [(json!("12.50"), Some("12.50")), (json!(7), Some("7")), (json!(null), None), (json!(""), None)] {
            let v = json!({"rateLimits": {"credits": {"hasCredits": true, "unlimited": false, "balance": raw}}});
            let snap = parse_rate_limits_result(&v, now()).unwrap();
            assert_eq!(snap.credits.unwrap().balance.as_deref(), expected);
        }
    }

    #[test]
    fn rpc_errors_with_login_wording_are_auth_required() {
        let rx = channel_with(&[
            r#"{"id":2,"error":{"code":-32000,"message":"failed to fetch codex rate limits: GET https://chatgpt.com/backend-api/wham/usage failed: 401 Unauthorized; content-type=text/plain; body={ \"error\": { \"message\": \"Encountered invalidated oauth token\" } }"}}"#,
        ]);
        assert!(matches!(wait_for_response(&rx, 2, soon()), Err(UsageFetchError::AuthRequired)));
        // A bare proxy 401 is not a login problem.
        let rx = channel_with(&[r#"{"id":2,"error":{"code":-32000,"message":"proxy returned 401 Unauthorized"}}"#]);
        assert!(matches!(wait_for_response(&rx, 2, soon()), Err(UsageFetchError::Rpc(_))));
    }

    #[test]
    fn secondary_buckets_never_enforce() {
        let mut snap = snap_with(&[(300, 38, Some(8000))]);
        snap.buckets.push(UsageBucket {
            limit_id: Some("premium".into()),
            limit_name: None,
            windows: vec![UsageWindow { window_minutes: Some(300), used_percent: 100, resets_at: Some(now()) }],
            rate_limit_reached_type: Some("workspace_owner_credits_depleted".into()),
        });
        assert_eq!(verdict(&snap, now()), UsageVerdict::Healthy);
        // Premium-only snapshot: nothing enforced.
        let premium_only = UsageSnapshot {
            fetched_at: now(),
            plan_type: Some("team".into()),
            buckets: vec![snap.buckets[1].clone()],
            credits: None,
            spend_control_reached: None,
            resets: None,
        };
        assert_eq!(verdict(&premium_only, now()), UsageVerdict::Healthy);
        assert!(enforcement_bucket(&premium_only).is_none());
        // Legacy single view (limitId null) is enforced.
        let mut legacy = snap_with(&[(300, 100, Some(600))]);
        legacy.buckets[0].limit_id = None;
        assert!(matches!(verdict(&legacy, now()), UsageVerdict::Exhausted { .. }));
    }

    #[test]
    fn verdict_reached_type_and_spend_control() {
        let mut snap = snap_with(&[(300, 60, Some(8000))]);
        snap.buckets[0].rate_limit_reached_type = Some("workspace_member_credits_depleted".into());
        assert_eq!(
            verdict(&snap, now()),
            UsageVerdict::Exhausted { reason: CooldownReason::Credits, resets_at: None }
        );
        snap.buckets[0].rate_limit_reached_type = Some("workspace_owner_usage_limit_reached".into());
        assert_eq!(
            verdict(&snap, now()),
            UsageVerdict::Exhausted { reason: CooldownReason::SpendControl, resets_at: None },
            "an admin-set workspace limit is workspace-wide and not lifted by credits"
        );
        snap.buckets[0].rate_limit_reached_type = Some("rate_limit_reached".into());
        assert_eq!(
            verdict(&snap, now()),
            UsageVerdict::Exhausted { reason: CooldownReason::RateLimit, resets_at: None }
        );
        snap.buckets[0].rate_limit_reached_type = None;
        snap.spend_control_reached = Some(true);
        assert_eq!(
            verdict(&snap, now()),
            UsageVerdict::Exhausted { reason: CooldownReason::SpendControl, resets_at: None }
        );
    }

    fn cfg_ws(seats: &[(&str, &str)]) -> SeatConfig {
        SeatConfig {
            seats: seats
                .iter()
                .map(|(n, a)| SeatEntry {
                    name: n.to_string(),
                    label: None,
                    account_id: Some(a.to_string()),
                    user_id: Some(format!("u-{}", n)),
                })
                .collect(),
            rotation: RotationConfig {
                cooldown_min_seconds: 60,
                cooldown_max_seconds: 86_400,
                cooldown_jitter_seconds: 0,
                default_cooldown_seconds: 3600,
                ..Default::default()
            },
        }
    }

    #[test]
    fn reconcile_personal_exhaustion_cools_only_that_seat_and_never_slides() {
        let cfg = cfg_ws(&[("a", "ws"), ("b", "ws")]);
        let mut state = SeatState::default();
        let snap = snap_with(&[(10080, 100, Some(7200))]);
        let n = reconcile_snapshots(&cfg, &mut state, vec![("a".into(), snap.clone())], now());
        let first = state.get("a").cooldown_until.unwrap();
        assert_eq!(first, now() + chrono::Duration::seconds(7200));
        assert!(n.iter().any(|(_, m)| m.contains("exhausted")));
        assert!(state.get("b").cooldown_until.is_none(), "personal limit stays personal");
        // Same observation later: the deadline must not move (the slide bug).
        for mins in [5, 30, 90] {
            let later = now() + chrono::Duration::minutes(mins);
            reconcile_snapshots(&cfg, &mut state, vec![("a".into(), snap.clone())], later);
            assert_eq!(state.get("a").cooldown_until, Some(first), "after {} min", mins);
        }
        // Beyond max clamp: once a reset is past the 24h cap the cooldown is
        // clamped once and still does not slide.
        let far = snap_with(&[(10080, 100, Some(5 * 86400))]);
        let mut st2 = SeatState::default();
        reconcile_snapshots(&cfg, &mut st2, vec![("a".into(), far.clone())], now());
        let clamped = st2.get("a").cooldown_until.unwrap();
        assert_eq!(clamped, now() + chrono::Duration::seconds(86_400));
        reconcile_snapshots(&cfg, &mut st2, vec![("a".into(), far)], now() + chrono::Duration::hours(3));
        assert_eq!(st2.get("a").cooldown_until, Some(clamped));
    }

    #[test]
    fn reconcile_credits_clear_clearable_cooldowns_across_workspace_only() {
        let cfg = cfg_ws(&[("a", "ws"), ("b", "ws"), ("c", "other")]);
        let mut state = SeatState::default();
        let until = now() + chrono::Duration::hours(20);
        for (n, r) in [("a", "rate_limit"), ("b", "credits"), ("c", "rate_limit")] {
            state.entry_mut(n).cooldown_until = Some(until);
            state.entry_mut(n).cooldown_reason = Some(r.into());
        }
        state.entry_mut("b").needs_login = true;
        let snap = with_credits(snap_with(&[(10080, 100, Some(7200))]));
        let n = reconcile_snapshots(&cfg, &mut state, vec![("a".into(), snap)], now());
        assert!(state.get("a").cooldown_until.is_none());
        assert!(state.get("b").cooldown_until.is_none(), "sibling in the same workspace cleared");
        assert!(state.get("b").needs_login, "needs_login is never touched");
        assert_eq!(state.get("c").cooldown_until, Some(until), "other workspace untouched");
        assert!(n.iter().filter(|(_, m)| m.contains("workspace credits available")).count() == 2);
        assert!(matches!(quota_state(&state.get("a"), now()), QuotaState::OnCredits { .. }));
    }

    #[test]
    fn reconcile_credits_do_not_clear_model_limit_or_spend_control() {
        let cfg = cfg_ws(&[("a", "ws"), ("b", "ws")]);
        let mut state = SeatState::default();
        let until = now() + chrono::Duration::hours(2);
        state.entry_mut("a").cooldown_until = Some(until);
        state.entry_mut("a").cooldown_reason = Some("model_limit".into());
        state.entry_mut("b").cooldown_until = Some(until);
        state.entry_mut("b").cooldown_reason = Some("spend_control".into());
        let snap = with_credits(snap_with(&[(10080, 10, Some(60))]));
        let n = reconcile_snapshots(&cfg, &mut state, vec![("a".into(), snap.clone()), ("b".into(), snap)], now());
        assert_eq!(state.get("a").cooldown_until, Some(until));
        assert_eq!(state.get("b").cooldown_until, Some(until));
        assert!(n.iter().any(|(_, m)| m.contains("--clear-cooldown a")));
    }

    #[test]
    fn reconcile_blocker_dominates_credits_in_either_order() {
        let cfg = cfg_ws(&[("a", "ws"), ("b", "ws")]);
        let mut blocked = snap_with(&[(10080, 50, Some(60))]);
        blocked.buckets[0].rate_limit_reached_type = Some("workspace_member_credits_depleted".into());
        let credits = with_credits(snap_with(&[(10080, 50, Some(60))]));
        for order in [
            vec![("a".to_string(), blocked.clone()), ("b".to_string(), credits.clone())],
            vec![("b".to_string(), credits.clone()), ("a".to_string(), blocked.clone())],
        ] {
            let mut state = SeatState::default();
            reconcile_snapshots(&cfg, &mut state, order, now());
            for s in ["a", "b"] {
                assert!(state.get(s).cooldown_until.is_some(), "{} cooled", s);
                assert_eq!(state.get(s).cooldown_reason.as_deref(), Some("credits"));
            }
        }
    }

    #[test]
    fn reconcile_snapshot_is_the_probe_after_expiry() {
        let cfg = cfg_ws(&[("a", "ws")]);
        let mut state = SeatState::default();
        let snap = snap_with(&[(10080, 100, Some(5 * 86400))]);
        reconcile_snapshots(&cfg, &mut state, vec![("a".into(), snap.clone())], now());
        let expiry = state.get("a").cooldown_until.unwrap();
        // Exactly at expiry the cooldown is no longer active; a fresh
        // snapshot that still shows 100% without credits starts a new one.
        reconcile_snapshots(&cfg, &mut state, vec![("a".into(), snap)], expiry);
        assert!(state.get("a").cooldown_until.unwrap() > expiry);
    }

    #[test]
    fn parse_reads_free_reset_grants() {
        let mk = |status: &str, exp_offset: i64, id: &str| {
            json!({"id": id, "resetType": "codexRateLimits", "status": status,
                   "grantedAt": now().timestamp() - 100,
                   "expiresAt": now().timestamp() + exp_offset,
                   "title": "Full reset (Weekly + 5 hr)"})
        };
        let v = json!({"rateLimits": {"primary": {"usedPercent": 5}},
            "rateLimitResetCredits": {"availableCount": 3, "credits": [
                mk("available", 900_000, "later"),
                mk("available", 300_000, "soonest"),
                mk("redeemed", 100, "spent"),
                mk("available", -60, "expired")]}});
        let snap = parse_rate_limits_result(&v, now()).unwrap();
        let r = snap.resets.clone().unwrap();
        assert_eq!(r.available, 3, "the backend's own count is reported");
        assert_eq!(r.next_expires_at, Some(now() + chrono::Duration::seconds(300_000)),
            "earliest expiry among redeemable grants (redeemed and expired ignored)");
        assert_eq!(r.next_title.as_deref(), Some("Full reset (Weekly + 5 hr)"));
        // --dry-run listing sees the same two, with ids.
        let listed = parse_reset_credits(&v, now());
        assert_eq!(listed.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["later", "soonest"]);
        // No section at all.
        let snap = parse_rate_limits_result(&json!({"rateLimits": {}}), now()).unwrap();
        assert!(snap.resets.is_none());
        assert!(parse_reset_credits(&json!({"rateLimits": {}}), now()).is_empty());
    }

    #[test]
    fn reset_outcomes_and_cost_formatting() {
        for (raw, parsed) in [
            ("reset", ResetOutcome::Reset),
            ("nothingToReset", ResetOutcome::NothingToReset),
            ("noCredit", ResetOutcome::NoCredit),
            ("alreadyRedeemed", ResetOutcome::AlreadyRedeemed),
        ] {
            assert_eq!(ResetOutcome::parse(raw), Some(parsed));
        }
        assert_eq!(ResetOutcome::parse("somethingNew"), None);
        assert!(ResetOutcome::Reset.is_reset() && !ResetOutcome::NothingToReset.is_reset());

        assert_eq!(
            format_cost(&ThreadCost { credits_micros: 420_000, usd_micros: Some(52_000) }),
            "≈0.42 credits (≈$0.05)"
        );
        assert_eq!(format_cost(&ThreadCost { credits_micros: 0, usd_micros: None }), "≈0.00 credits");
        assert_eq!(
            format_cost(&ThreadCost { credits_micros: 12_500_000, usd_micros: None }),
            "≈12.50 credits"
        );
        assert_eq!(
            parse_thread_cost(&json!({"threadUsage": {"estimatedUsageCreditsMicros": 1, "estimatedUsageUsdMicros": null}})),
            Some(ThreadCost { credits_micros: 1, usd_micros: None })
        );
        assert!(parse_thread_cost(&json!({"threadUsage": {}})).is_none());
        assert_eq!(format_tokens(1_329_690_061), "1.33B");
        assert_eq!(format_tokens(46_303_727), "46.3M");
        assert_eq!(format_tokens(912), "912");

        let u = parse_account_usage(&json!({"summary": {"lifetimeTokens": 100},
            "dailyUsageBuckets": [{"startDate":"2026-09-01","tokens":1},{"startDate":"2026-09-02","tokens":2},
                                  {"startDate":"2026-09-03","tokens":4},{"startDate":"2026-09-04","tokens":8},
                                  {"startDate":"2026-09-05","tokens":16},{"startDate":"2026-09-06","tokens":32},
                                  {"startDate":"2026-09-07","tokens":64},{"startDate":"2026-09-08","tokens":128}]}));
        assert_eq!(u.lifetime_tokens, Some(100));
        assert_eq!(u.last_7d_tokens, 2 + 4 + 8 + 16 + 32 + 64 + 128, "last seven buckets only");
    }

    #[test]
    fn clear_after_reset_only_lifts_what_a_reset_lifts() {
        let mut state = SeatState::default();
        // Cooling for a window limit, and the refreshed reading is healthy.
        state.entry_mut("a").cooldown_until = Some(now() + chrono::Duration::hours(20));
        state.entry_mut("a").cooldown_reason = Some("rate_limit".into());
        state.entry_mut("a").usage = Some(snap_with(&[(300, 2, Some(600))]));
        assert!(clear_window_cooldown_after_reset(&mut state, "a", now()));
        assert!(state.get("a").cooldown_until.is_none());

        // A credits or spend-cap cooldown is not something a reset lifts.
        for reason in ["credits", "spend_control"] {
            let mut state = SeatState::default();
            state.entry_mut("a").cooldown_until = Some(now() + chrono::Duration::hours(20));
            state.entry_mut("a").cooldown_reason = Some(reason.into());
            state.entry_mut("a").usage = Some(snap_with(&[(300, 2, Some(600))]));
            assert!(!clear_window_cooldown_after_reset(&mut state, "a", now()), "{}", reason);
            assert!(state.get("a").cooldown_until.is_some(), "{}", reason);
        }
        // A reading that still says exhausted keeps the cooldown.
        let mut state = SeatState::default();
        state.entry_mut("a").cooldown_until = Some(now() + chrono::Duration::hours(20));
        state.entry_mut("a").cooldown_reason = Some("rate_limit".into());
        state.entry_mut("a").usage = Some(snap_with(&[(10080, 100, Some(600))]));
        assert!(!clear_window_cooldown_after_reset(&mut state, "a", now()));
        // Cached reset counts survive a state round-trip.
        let mut snap = snap_with(&[(300, 1, None)]);
        snap.resets = Some(UsageResets { available: 2, next_expires_at: Some(now()), next_title: Some("t".into()) });
        let mut st = SeatState::default();
        st.entry_mut("a").usage = Some(snap);
        let back: SeatState = serde_json::from_str(&serde_json::to_string(&st).unwrap()).unwrap();
        assert_eq!(back.get("a").usage.unwrap().resets.unwrap().available, 2);
        let legacy: SeatState = serde_json::from_str(r#"{"seats":{"a":{"usage":{"fetched_at":"2026-09-01T00:00:00Z"}}}}"#).unwrap();
        assert!(legacy.get("a").usage.unwrap().resets.is_none());
    }

    #[test]
    fn reconcile_reports_secondary_bucket_notices() {
        let cfg = cfg_ws(&[("a", "ws")]);
        let mut state = SeatState::default();
        let mut snap = snap_with(&[(300, 38, Some(8000))]);
        snap.buckets.push(UsageBucket {
            limit_id: Some("premium".into()),
            limit_name: None,
            windows: vec![],
            rate_limit_reached_type: Some("workspace_owner_credits_depleted".into()),
        });
        let n = reconcile_snapshots(&cfg, &mut state, vec![("a".into(), snap)], now());
        assert!(state.get("a").cooldown_until.is_none());
        assert!(n.iter().any(|(_, m)| m.contains("'premium' limit reports workspace_owner_credits_depleted")));
    }
}
