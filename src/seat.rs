//! Multi-seat OAuth management for the codex CLI.
//!
//! Each seat is a ChatGPT account whose OAuth blob we keep in
//! `~/.config/codex-clean/seats/<name>/auth.json`. Before each codex run we
//! swap the chosen seat's blob into `~/.codex/auth.json`; after the run we
//! copy it back so codex's own token-refresh writes are persisted.

use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Codex's home directory (where it reads/writes auth.json + sessions).
/// Honours `$CODEX_HOME` if the user has it set; defaults to `~/.codex`.
pub fn codex_home() -> Result<PathBuf> {
    if let Ok(p) = env::var("CODEX_HOME") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    Ok(home_dir()?.join(".codex"))
}

/// `~/.codex/auth.json`.
pub fn codex_auth_path() -> Result<PathBuf> {
    Ok(codex_home()?.join("auth.json"))
}

/// `~/.codex/config.toml`.
pub fn codex_config_path() -> Result<PathBuf> {
    Ok(codex_home()?.join("config.toml"))
}

/// `~/.config/codex-clean/`. We use XDG-style explicitly rather than
/// `dirs::config_dir()` (which would pick `~/Library/Application Support` on
/// macOS) because the plan says `~/.config/codex-clean/` on all platforms.
///
/// `$CODEX_CLEAN_HOME` overrides this entirely — used by integration tests
/// to redirect the side store to a tempdir without touching the user's real
/// `~/.config/codex-clean`.
pub fn config_dir() -> Result<PathBuf> {
    if let Ok(p) = env::var("CODEX_CLEAN_HOME") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    Ok(home_dir()?.join(".config").join("codex-clean"))
}

pub fn seats_toml_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("seats.toml"))
}

pub fn state_json_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("state.json"))
}

pub fn seats_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join("seats"))
}

pub fn seat_auth_path(name: &str) -> Result<PathBuf> {
    Ok(seats_dir()?.join(name).join("auth.json"))
}

pub fn lock_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("codex.lock"))
}

pub fn unmatched_log_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("unmatched.log"))
}

/// `~/.config/codex-clean/orphaned/` — where a refresh-back that fails the
/// identity guard parks the foreign auth blob instead of destroying it.
pub fn orphaned_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join("orphaned"))
}

/// `~/.config/codex-clean/seat-events.log` — append-only record of the
/// things a background caller would otherwise never see: limits hit, auth
/// failures, cooldowns, orphaned blobs, logins. Survives `seat remove` /
/// `seat login`, unlike `state.json`.
pub fn seat_events_log_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("seat-events.log"))
}

/// Logs under the side store are rotated once (to `<name>.1`) when they pass
/// this size, so background use cannot grow them without bound.
pub const PRIVATE_LOG_ROTATE_BYTES: u64 = 1024 * 1024;

/// Append one entry to a 0600 log file under the side store. Creates the
/// parent (0700) and the file (0600) as needed. A pre-existing file with
/// looser permissions is tightened through the open descriptor; if that
/// cannot be confirmed the write is **refused** — these logs hold model and
/// error text and must never fail open. Rotates at [`PRIVATE_LOG_ROTATE_BYTES`].
pub fn append_private_log(path: &Path, entry: &str) -> Result<()> {
    if let Some(p) = path.parent() {
        secure_create_dir_all(p)?;
    }
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            anyhow::bail!("{} is a symlink; refusing to write through it", path.display());
        }
        if meta.len() >= PRIVATE_LOG_ROTATE_BYTES {
            // Rotation must succeed or the write is refused: appending to an
            // oversized log would defeat the bound. Callers treat logging as
            // best-effort, so an error here only drops the entry.
            let rotated = path.with_extension(
                format!("{}.1", path.extension().and_then(|e| e.to_str()).unwrap_or("log")),
            );
            match fs::remove_file(&rotated) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(anyhow::Error::from(e)
                        .context(format!("removing old rotated log {}", rotated.display())))
                }
            }
            fs::rename(path, &rotated).with_context(|| {
                format!("rotating {} to {}", path.display(), rotated.display())
            })?;
        }
    }
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = f.metadata().with_context(|| format!("inspecting {}", path.display()))?;
        if meta.permissions().mode() & 0o077 != 0 {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            f.set_permissions(perms)
                .with_context(|| format!("tightening permissions on {}", path.display()))?;
            let after = f.metadata().with_context(|| format!("re-inspecting {}", path.display()))?;
            if after.permissions().mode() & 0o077 != 0 {
                anyhow::bail!(
                    "{} is readable by others and could not be tightened; refusing to write",
                    path.display()
                );
            }
        }
    }
    f.write_all(entry.as_bytes())
        .with_context(|| format!("writing to {}", path.display()))
}

/// Cap a string to `max` characters (appending an ellipsis) with control
/// characters replaced, for log fields fed from codex output.
pub fn log_excerpt(s: &str, max: usize) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.chars().count() <= max {
        trimmed.to_string()
    } else {
        format!("{}…", trimmed.chars().take(max).collect::<String>())
    }
}

/// Seats that share `seat`'s workspace (`account_id`), including itself. A
/// seat with no recorded account_id is its own workspace.
pub fn workspace_siblings(config: &SeatConfig, seat: &str) -> Vec<String> {
    let Some(account) = config.find(seat).and_then(|s| s.account_id.clone()) else {
        return vec![seat.to_string()];
    };
    let mut out: Vec<String> = config
        .seats
        .iter()
        .filter(|s| s.account_id.as_deref() == Some(account.as_str()))
        .map(|s| s.name.clone())
        .collect();
    if !out.iter().any(|n| n == seat) {
        out.push(seat.to_string());
    }
    out
}

/// Put `names` into cooldown until `until` for `reason`, **extend-only**: a
/// seat already cooling for longer keeps its later deadline and its own
/// reason. Returns the names whose cooldown actually changed.
pub fn cool_seats(
    state: &mut SeatState,
    names: &[String],
    until: DateTime<Utc>,
    reason: &str,
    now: DateTime<Utc>,
) -> Vec<String> {
    let mut changed = Vec::new();
    for name in names {
        let entry = state.entry_mut(name);
        let existing = entry.cooldown_until.filter(|u| *u > now);
        if existing.is_none_or(|u| until > u) {
            entry.cooldown_until = Some(until);
            entry.cooldown_reason = Some(reason.to_string());
            changed.push(name.clone());
        }
    }
    changed
}

/// Record a seat event: `<utc-ts> <kind> seat=<name> <detail>` on one line.
/// Best effort — logging must never fail a run.
pub fn log_event(kind: &str, seat: &str, detail: &str) {
    let detail: String = detail
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let line = format!(
        "{} {} seat={} {}\n",
        Utc::now().to_rfc3339(),
        kind,
        seat,
        log_excerpt(&detail, 600).replace('\n', " ")
    );
    if let Ok(path) = seat_events_log_path() {
        let _ = append_private_log(&path, &line);
    }
}

/// One-line summary of anything degraded about the seat pool, for stdout at
/// the end of every run. `None` when every seat is usable. Background
/// callers (agents, CI) read stdout, so this is where a "you need to act"
/// signal has to live — a one-off stderr warning is never seen.
pub fn seat_notice(config: &SeatConfig, state: &SeatState, now: DateTime<Utc>) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut usable = 0usize;
    for s in &config.seats {
        let st = state.get(&s.name);
        if st.needs_login {
            parts.push(format!(
                "{} needs login (run: codex-clean seat login {})",
                s.name, s.name
            ));
        } else if let Some(until) = st.cooldown_until.filter(|u| *u > now) {
            let reason = st
                .cooldown_reason
                .as_deref()
                .filter(|r| *r != "rate_limit")
                .map(|r| format!(", {}", r.replace('_', " ")))
                .unwrap_or_default();
            parts.push(format!(
                "{} cooling until {}{}",
                s.name,
                until.with_timezone(&chrono::Local).format("%a %H:%M"),
                reason
            ));
        } else {
            usable += 1;
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!(
        "Seats: {} — {} of {} usable",
        parts.join("; "),
        usable,
        config.seats.len()
    ))
}

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home directory"))
}

/// Env vars we strip from any codex child process so the swapped auth.json
/// is the only credential in scope. `CODEX_HOME` is *not* on this list: we
/// honour the user's setting and use it as the swap target.
pub(crate) const SCRUB_ENV_VARS: &[&str] = &[
    "CODEX_SQLITE_HOME",
    "OPENAI_API_KEY",
    "CODEX_INTERNAL_ORIGINATOR_OVERRIDE",
    "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
    "CODEX_SANDBOX",
    "CODEX_CLEAN_SEAT",
];

// ---------------------------------------------------------------------------
// Seat names
// ---------------------------------------------------------------------------

/// Seat names become directory names under `seats/`, so they must be
/// path-safe. Enforced on `seat add` and again on every `seats.toml` load so
/// a hand-edited file cannot smuggle a `..` or a `/` into a filesystem path.
pub fn validate_seat_name(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("seat name cannot be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!(
            "seat name '{}' contains invalid characters (use [a-zA-Z0-9_-])",
            name
        );
    }
    if name == "." || name == ".." {
        anyhow::bail!("seat name '{}' is reserved", name);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Config (seats.toml)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SeatConfig {
    #[serde(default, rename = "seat")]
    pub seats: Vec<SeatEntry>,
    #[serde(default)]
    pub rotation: RotationConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SeatEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// `tokens.account_id` from this seat's `auth.json` at registration
    /// time. Used to verify that a re-login is for the same ChatGPT
    /// account, so a slip-of-the-finger doesn't silently install the wrong
    /// account's tokens into this seat's slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// User-level identity (`chatgpt_user_id`, falling back to `sub`) from the
    /// seat's id token. Two seats in the same Team workspace share an
    /// `account_id`, so this is what actually tells them apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

impl SeatEntry {
    pub fn identity(&self) -> SeatIdentity {
        SeatIdentity {
            account_id: self.account_id.clone(),
            user_id: self.user_id.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RotationConfig {
    #[serde(default)]
    pub strategy: Strategy,
    /// Seat preferred by the `fixed` strategy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_seat: Option<String>,
    /// For the `balanced` strategy: a seat whose recorded usage snapshot is
    /// older than this is refreshed (via `codex app-server`) before picking.
    #[serde(default = "default_balance_refresh")]
    pub balance_refresh_seconds: u64,
    #[serde(default = "default_default_cooldown")]
    pub default_cooldown_seconds: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_min_cooldown")]
    pub cooldown_min_seconds: u64,
    #[serde(default = "default_max_cooldown")]
    pub cooldown_max_seconds: u64,
    #[serde(default = "default_jitter")]
    pub cooldown_jitter_seconds: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    /// The eligible seat used longest ago. With healthy seats this
    /// alternates like round-robin.
    #[default]
    LeastRecentlyUsed,
    /// The eligible seat after the active one, in declaration order.
    RoundRobin,
    /// Always `fixed_seat` while it is eligible; otherwise the
    /// least-recently-used eligible seat. (Strict pinning with no fallback
    /// is `CODEX_CLEAN_SEAT`.)
    Fixed,
    /// The eligible seat with the most headroom on its tightest usage
    /// window, so usage stays level across seats. Uses the snapshot recorded
    /// by `seat status`, refreshed automatically when older than
    /// `balance_refresh_seconds`; a seat with no snapshot counts as unused.
    Balanced,
}

impl Strategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LeastRecentlyUsed => "least-recently-used",
            Self::RoundRobin => "round-robin",
            Self::Fixed => "fixed",
            Self::Balanced => "balanced",
        }
    }

    /// Parse a user-supplied name; accepts the kebab-case names plus the
    /// short aliases `lru` and `rr`.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "least-recently-used" | "lru" => Self::LeastRecentlyUsed,
            "round-robin" | "rr" => Self::RoundRobin,
            "fixed" => Self::Fixed,
            "balanced" | "balance" => Self::Balanced,
            _ => return None,
        })
    }
}

impl std::fmt::Display for Strategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

fn default_balance_refresh() -> u64 { 1800 }
fn default_default_cooldown() -> u64 { 3600 }
fn default_max_retries() -> u32 { 1 }
fn default_min_cooldown() -> u64 { 300 }
fn default_max_cooldown() -> u64 { 86400 }
fn default_jitter() -> u64 { 120 }

impl Default for RotationConfig {
    fn default() -> Self {
        Self {
            strategy: Strategy::LeastRecentlyUsed,
            fixed_seat: None,
            balance_refresh_seconds: default_balance_refresh(),
            default_cooldown_seconds: default_default_cooldown(),
            max_retries: default_max_retries(),
            cooldown_min_seconds: default_min_cooldown(),
            cooldown_max_seconds: default_max_cooldown(),
            cooldown_jitter_seconds: default_jitter(),
        }
    }
}

impl RotationConfig {
    /// Reject configurations that would panic later (notably `min > max` which
    /// blows `u64::clamp`).
    pub fn validate(&self) -> Result<()> {
        if self.cooldown_min_seconds > self.cooldown_max_seconds {
            anyhow::bail!(
                "rotation.cooldown_min_seconds ({}) must be <= rotation.cooldown_max_seconds ({})",
                self.cooldown_min_seconds,
                self.cooldown_max_seconds
            );
        }
        Ok(())
    }
}

impl SeatConfig {
    pub fn load() -> Result<Option<Self>> {
        let path = seats_toml_path()?;
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: SeatConfig = toml::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate().with_context(|| {
            format!(
                "invalid configuration in {}. To rename a seat by hand: change its `name` in \
                 seats.toml, rename {}/<old> to the new name, and update the matching key (and \
                 `active_seat`) in {}",
                path.display(),
                seats_dir().map(|p| p.display().to_string()).unwrap_or_default(),
                state_json_path().map(|p| p.display().to_string()).unwrap_or_default()
            )
        })?;
        cfg.backfill_identities();
        Ok(Some(cfg))
    }

    /// Validate rotation bounds and every seat name (path-safe, unique).
    /// Uniqueness is ASCII-case-insensitive: seat names become directory
    /// names, and macOS / Windows filesystems fold case, so `Work` and `work`
    /// would share one slot.
    pub fn validate(&self) -> Result<()> {
        self.rotation.validate()?;
        let mut seen = std::collections::BTreeSet::new();
        for s in &self.seats {
            validate_seat_name(&s.name)?;
            if !seen.insert(s.name.to_ascii_lowercase()) {
                anyhow::bail!(
                    "duplicate seat name '{}' (names are compared case-insensitively because \
                     seat directories may live on a case-insensitive filesystem)",
                    s.name
                );
            }
        }
        if self.rotation.strategy == Strategy::Fixed {
            match &self.rotation.fixed_seat {
                None => anyhow::bail!(
                    "rotation.strategy = \"fixed\" requires rotation.fixed_seat = \"<name>\" \
                     (run `codex-clean seat strategy fixed <name>`)"
                ),
                Some(f) if self.find(f).is_none() => anyhow::bail!(
                    "rotation.fixed_seat = \"{}\" is not a configured seat",
                    f
                ),
                _ => {}
            }
        }
        Ok(())
    }

    /// Case-insensitive lookup, for rejecting `seat add Work` when `work` exists.
    pub fn find_case_insensitive(&self, name: &str) -> Option<&SeatEntry> {
        self.seats.iter().find(|s| s.name.eq_ignore_ascii_case(name))
    }

    /// Seats registered before `user_id` existed have only an `account_id`.
    /// Fill the gap in memory from the slot's own auth blob so identity
    /// guards work immediately; the next `save()` persists it.
    fn backfill_identities(&mut self) {
        for s in &mut self.seats {
            if s.account_id.is_some() && s.user_id.is_some() {
                continue;
            }
            let Ok(path) = seat_auth_path(&s.name) else { continue };
            let Ok(bytes) = fs::read(&path) else { continue };
            let Ok(id) = read_identity(&bytes) else { continue };
            if s.account_id.is_none() {
                s.account_id = id.account_id;
            }
            if s.user_id.is_none() {
                s.user_id = id.user_id;
            }
        }
    }

    /// Identity the guards should expect for `name`.
    pub fn identity_for(&self, name: &str) -> SeatIdentity {
        self.find(name).map(|s| s.identity()).unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        // Never persist something `load` would refuse: that would lock every
        // subcommand out, including the one needed to repair it.
        self.validate().context("refusing to save an invalid seats.toml")?;
        let path = seats_toml_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(self).context("serialising seats.toml")?;
        atomic_write(&path, raw.as_bytes())
    }

    pub fn find(&self, name: &str) -> Option<&SeatEntry> {
        self.seats.iter().find(|s| s.name == name)
    }
}

// ---------------------------------------------------------------------------
// Runtime state (state.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SeatState {
    #[serde(default)]
    pub seats: BTreeMap<String, SeatRuntimeState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_seat: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SeatRuntimeState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<DateTime<Utc>>,
    /// Why `cooldown_until` was set: `rate_limit`, `credits`, `spend_control`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_reason: Option<String>,
    #[serde(default)]
    pub needs_login: bool,
    #[serde(default)]
    pub consecutive_failures: u32,
    /// Last quota snapshot recorded by `seat status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageSnapshot>,
}

impl SeatRuntimeState {
    /// Eligible for rotation: logged in and not cooling.
    pub fn is_eligible(&self, now: DateTime<Utc>) -> bool {
        !self.needs_login && self.cooldown_until.is_none_or(|u| u <= now)
    }

    /// Used-percent of the seat's tightest window on its enforced limit,
    /// from the recorded snapshot. `None` when no snapshot has been taken.
    /// The `balanced` strategy prefers the smallest value.
    pub fn usage_score(&self) -> Option<u32> {
        let snap = self.usage.as_ref()?;
        // Only the enforced limit counts (mirrors usage::enforcement_bucket);
        // a model-specific bucket such as `premium` says nothing about the
        // seat's general headroom.
        let bucket = snap
            .buckets
            .iter()
            .find(|b| b.limit_id.as_deref() == Some("codex"))
            .or_else(|| snap.buckets.iter().find(|b| b.limit_id.is_none()))?;
        bucket.windows.iter().map(|w| w.used_percent).max()
    }

    /// True when the snapshot is missing or older than `max_age`.
    pub fn usage_is_stale(&self, now: DateTime<Utc>, max_age: chrono::Duration) -> bool {
        match &self.usage {
            None => true,
            Some(u) => now - u.fetched_at > max_age,
        }
    }
}

// ---------------------------------------------------------------------------
// Usage snapshot (recorded in state.json by `seat status`)
// ---------------------------------------------------------------------------

/// One metered window (e.g. the 5-hour or weekly limit).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UsageWindow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u64>,
    pub used_percent: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<DateTime<Utc>>,
}

/// One rate-limit bucket (codex reports one per metered `limit_id`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UsageBucket {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_name: Option<String>,
    #[serde(default)]
    pub windows: Vec<UsageWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_reached_type: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UsageCredits {
    pub has_credits: bool,
    pub unlimited: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UsageSnapshot {
    pub fetched_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
    #[serde(default)]
    pub buckets: Vec<UsageBucket>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<UsageCredits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend_control_reached: Option<bool>,
}

impl SeatState {
    pub fn load() -> Result<Self> {
        let path = state_json_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        let state: SeatState = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(state)
    }

    pub fn save(&self) -> Result<()> {
        let path = state_json_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let raw = serde_json::to_string_pretty(self).context("serialising state.json")?;
        atomic_write(&path, raw.as_bytes())
    }

    pub fn entry_mut(&mut self, name: &str) -> &mut SeatRuntimeState {
        self.seats
            .entry(name.to_string())
            .or_default()
    }

    pub fn get(&self, name: &str) -> SeatRuntimeState {
        self.seats.get(name).cloned().unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Atomic write helper
// ---------------------------------------------------------------------------

/// Write `data` to `path` atomically: write to a sibling temp file, fsync,
/// then rename. On Unix, both the temp and final files are created with
/// mode 0600 to keep OAuth tokens and other secrets readable only by the
/// owner. The parent directory is fsynced after the rename so the directory
/// entry survives a crash.
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        secure_create_dir_all(parent)?;
    }
    let pid = std::process::id();
    let tmp = path.with_extension(format!("tmp.{}", pid));

    let mut opts = OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }

    let result = (|| -> Result<()> {
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("opening {}", tmp.display()))?;
        f.write_all(data)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync {}", tmp.display()))?;
        Ok(())
    })();

    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;

    // No post-rename chmod is needed: rename(2) replaces the destination
    // entry with the temp file's inode, so the surviving file has the temp
    // file's perms (0o600 from the OpenOptions::mode above on Unix).

    // fsync the parent directory so the rename is durable across crashes.
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// Create `path` (and parents) with restrictive permissions on Unix.
///
/// Only directories that this call actually *creates* are tightened to mode
/// 0700; any directory that already existed (including the leaf) is left
/// untouched. That preserves the original intent — fresh seat directories
/// shouldn't be world-readable — while making it safe to call with a path
/// rooted outside the user's home (e.g. `CODEX_CLEAN_HOME=/tmp/...` in tests,
/// or `path == config_dir`). We never chmod a directory we didn't just make.
///
/// `lstat` is used so a symlink in the path can't redirect the chmod onto
/// the link target.
pub fn secure_create_dir_all(path: &Path) -> Result<()> {
    // Snapshot the deepest pre-existing ancestor *before* we create anything;
    // anything strictly below this point is what create_dir_all is about to
    // create, and only those should be tightened.
    let pre_existing_root = first_existing_ancestor(path);

    fs::create_dir_all(path)
        .with_context(|| format!("creating {}", path.display()))?;

    #[cfg(unix)]
    {
        let mut cur: Option<&Path> = Some(path);
        while let Some(p) = cur {
            // Stop the moment we reach the first directory that was already
            // there before this call. Don't touch its perms.
            if pre_existing_root.as_deref() == Some(p) {
                break;
            }
            chmod_0700_if_loose_no_follow(p);
            cur = p.parent().filter(|pp| !pp.as_os_str().is_empty());
        }
    }
    Ok(())
}

/// Walk up from `path`, returning the deepest ancestor (or `path` itself)
/// that already exists, or `None` if the entire chain is missing.
fn first_existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(path);
    while let Some(p) = cur {
        if p.exists() {
            return Some(p.to_path_buf());
        }
        cur = p.parent().filter(|pp| !pp.as_os_str().is_empty());
    }
    None
}

#[cfg(unix)]
fn chmod_0700_if_loose_no_follow(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    // lstat: if `p` is a symlink we leave it alone rather than chmod'ing
    // through to the target.
    let Ok(meta) = fs::symlink_metadata(p) else { return };
    if meta.file_type().is_symlink() {
        return;
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode != 0 && mode != 0o700 && (mode & 0o077) != 0 {
        let mut perms = meta.permissions();
        perms.set_mode(0o700);
        let _ = fs::set_permissions(p, perms);
    }
}

// ---------------------------------------------------------------------------
// Lockfile (codex.lock)
// ---------------------------------------------------------------------------

/// Exclusive advisory lock held while a codex process runs. Drops release
/// the lock automatically; if the process is killed the OS releases it too.
pub struct CodexLock {
    file: File,
}

impl CodexLock {
    fn open_lock_file() -> Result<(File, PathBuf)> {
        let path = lock_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        Ok((file, path))
    }

    /// Acquire the lock, blocking until it's available.
    pub fn acquire() -> Result<Self> {
        let (file, path) = Self::open_lock_file()?;
        file.lock_exclusive()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(Self { file })
    }

    /// Non-blocking acquire. `Ok(None)` means another codex-clean holds it.
    pub fn try_acquire() -> Result<Option<Self>> {
        let (file, path) = Self::open_lock_file()?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { file })),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(anyhow::Error::from(e).context(format!("locking {}", path.display()))),
        }
    }
}

impl Drop for CodexLock {
    fn drop(&mut self) {
        // Name the trait explicitly: std gained an inherent `File::unlock` in
        // 1.89 and we declare an older MSRV.
        let _ = FileExt::unlock(&self.file);
    }
}

// ---------------------------------------------------------------------------
// Seat selection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum SeatPickError {
    NoSeatsConfigured,
    SeatNotFound(String),
    SeatNeedsLogin(String),
    SeatCooling { name: String, until: DateTime<Utc> },
    AllSeatsBlocked {
        soonest_name: Option<String>,
        soonest_until: Option<DateTime<Utc>>,
        /// How many seats are ineligible because they are cooling.
        cooling: usize,
        /// How many seats are ineligible because they need login.
        needs_login: usize,
    },
}

impl std::fmt::Display for SeatPickError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSeatsConfigured => write!(f, "no seats configured; run `codex-clean seat add <name>`"),
            Self::SeatNotFound(n) => write!(f, "seat '{}' not found", n),
            Self::SeatNeedsLogin(n) => write!(
                f,
                "seat '{}' needs login; run `codex-clean seat login {}`",
                n, n
            ),
            Self::SeatCooling { name, until } => write!(
                f,
                "seat '{}' is cooling until {}",
                name,
                until.with_timezone(&chrono::Local).format("%-I:%M %p")
            ),
            Self::AllSeatsBlocked {
                soonest_name,
                soonest_until,
                cooling,
                needs_login,
            } => {
                if *cooling == 0 {
                    return write!(
                        f,
                        "all {} seat(s) need login; run `codex-clean seat login <name>`",
                        needs_login
                    );
                }
                write!(f, "{} seat(s) cooling", cooling)?;
                if let (Some(n), Some(u)) = (soonest_name, soonest_until) {
                    write!(
                        f,
                        "; soonest available at {} (seat '{}')",
                        u.with_timezone(&chrono::Local).format("%-I:%M %p"),
                        n
                    )?;
                }
                if *needs_login > 0 {
                    write!(
                        f,
                        "; {} need login (run `codex-clean seat login <name>`)",
                        needs_login
                    )?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for SeatPickError {}

/// Pick a seat for the next codex invocation.
///
/// `override_seat` is the value of `CODEX_CLEAN_SEAT` if the caller passed
/// it. When set, we honour it strictly (no rotation, but we surface clear
/// errors if it's unusable). Without an override, rotation policy applies.
pub fn pick_seat(
    config: &SeatConfig,
    state: &SeatState,
    override_seat: Option<&str>,
    now: DateTime<Utc>,
) -> Result<String, SeatPickError> {
    pick_seat_excluding(config, state, override_seat, now, &[])
}

/// `pick_seat`, but seats named in `exclude` (already tried in this run) are
/// passed over while any other eligible seat remains. If every eligible seat
/// is excluded the exclusion is ignored, so the caller's "already tried"
/// guard still fires and the pool-exhausted logic runs as before.
pub fn pick_seat_excluding(
    config: &SeatConfig,
    state: &SeatState,
    override_seat: Option<&str>,
    now: DateTime<Utc>,
    exclude: &[String],
) -> Result<String, SeatPickError> {
    if config.seats.is_empty() {
        return Err(SeatPickError::NoSeatsConfigured);
    }

    if let Some(name) = override_seat {
        let seat = config
            .find(name)
            .ok_or_else(|| SeatPickError::SeatNotFound(name.to_string()))?;
        let st = state.get(&seat.name);
        if st.needs_login {
            return Err(SeatPickError::SeatNeedsLogin(seat.name.clone()));
        }
        if let Some(until) = st.cooldown_until {
            if until > now {
                return Err(SeatPickError::SeatCooling { name: seat.name.clone(), until });
            }
        }
        return Ok(seat.name.clone());
    }

    // Eligible = not needs_login, and either no cooldown or cooldown elapsed.
    let all_eligible: Vec<&SeatEntry> = config
        .seats
        .iter()
        .filter(|s| state.get(&s.name).is_eligible(now))
        .collect();

    if all_eligible.is_empty() {
        return Err(all_blocked_error(config, state));
    }
    let excluded: std::collections::HashSet<&str> = exclude.iter().map(String::as_str).collect();
    let untried: Vec<&SeatEntry> = all_eligible
        .iter()
        .copied()
        .filter(|s| !excluded.contains(s.name.as_str()))
        .collect();
    let eligible = if untried.is_empty() { all_eligible } else { untried };

    let lru = |pool: &[&SeatEntry]| -> String {
        // Smallest last_used (None sorts as oldest).
        pool.iter()
            .min_by_key(|s| state.get(&s.name).last_used)
            .expect("eligible non-empty")
            .name
            .clone()
    };

    let chosen = match config.rotation.strategy {
        Strategy::LeastRecentlyUsed => return Ok(lru(&eligible)),
        Strategy::Fixed => {
            let fixed = config.rotation.fixed_seat.as_deref().unwrap_or_default();
            if let Some(s) = eligible.iter().find(|s| s.name == fixed) {
                return Ok(s.name.clone());
            }
            // Preferred seat is cooling / needs login: fall back to LRU
            // among the rest so work still gets done.
            return Ok(lru(&eligible));
        }
        Strategy::Balanced => {
            // Most headroom on the tightest window wins; unknown usage counts
            // as 0 (the run's outcome corrects it); ties go to LRU.
            let best = eligible
                .iter()
                .min_by_key(|s| {
                    let st = state.get(&s.name);
                    (st.usage_score().unwrap_or(0), st.last_used)
                })
                .expect("eligible non-empty");
            return Ok(best.name.clone());
        }
        Strategy::RoundRobin => {
            // Pick the seat after the active seat in declaration order.
            // If active seat unknown or not in eligible list, pick the first eligible.
            let active = state.active_seat.as_deref();
            let idx = active
                .and_then(|a| config.seats.iter().position(|s| s.name == a))
                .map(|i| i + 1)
                .unwrap_or(0);
            // Walk forward from idx, wrapping, picking first eligible.
            let names: Vec<&str> = eligible.iter().map(|s| s.name.as_str()).collect();
            let n = config.seats.len();
            let mut pick = None;
            for k in 0..n {
                let candidate = &config.seats[(idx + k) % n];
                if names.contains(&candidate.name.as_str()) {
                    pick = Some(candidate);
                    break;
                }
            }
            pick.expect("eligible non-empty")
        }
    };

    Ok(chosen.name.clone())
}

/// Build the `AllSeatsBlocked` error for the current state: counts cooling
/// vs needs-login seats and finds the soonest cooldown expiry (ignoring
/// needs_login seats — those need user action, not time).
pub fn all_blocked_error(config: &SeatConfig, state: &SeatState) -> SeatPickError {
    let mut cooling = 0usize;
    let mut needs_login = 0usize;
    let mut soonest: Option<(String, DateTime<Utc>)> = None;
    for s in &config.seats {
        let st = state.get(&s.name);
        if st.needs_login {
            needs_login += 1;
            continue;
        }
        if let Some(u) = st.cooldown_until {
            cooling += 1;
            if soonest.as_ref().is_none_or(|(_, cur)| u < *cur) {
                soonest = Some((s.name.clone(), u));
            }
        }
    }
    SeatPickError::AllSeatsBlocked {
        soonest_name: soonest.as_ref().map(|(n, _)| n.clone()),
        soonest_until: soonest.map(|(_, u)| u),
        cooling,
        needs_login,
    }
}

// ---------------------------------------------------------------------------
// Auth.json swap + refresh-back
// ---------------------------------------------------------------------------

/// Copy `seats/<name>/auth.json` to `~/.codex/auth.json` atomically. Skips
/// the write when the active blob already matches (byte-equal).
pub fn swap_active_auth(name: &str) -> Result<()> {
    let src = seat_auth_path(name)?;
    if !src.exists() {
        return Err(anyhow!(
            "seat '{}' has no auth.json at {} (was the seat ever logged in?)",
            name,
            src.display()
        ));
    }
    let dst = codex_auth_path()?;
    let src_bytes = fs::read(&src)
        .with_context(|| format!("reading {}", src.display()))?;
    if let Ok(dst_bytes) = fs::read(&dst) {
        if dst_bytes == src_bytes {
            return Ok(());
        }
    }
    atomic_write(&dst, &src_bytes)
}

/// Result of a guarded refresh-back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshBackOutcome {
    /// The source blob was copied into the seat's slot.
    Copied,
    /// Slot already held identical bytes.
    Unchanged,
    /// The source file does not exist (codex never ran, or auth was wiped).
    SkippedNoSource,
    /// The source is not parseable as auth.json. Nothing written to the slot;
    /// the bytes were preserved at `orphaned` (if that succeeded).
    SkippedUnparseable { orphaned: Option<PathBuf> },
    /// Either side lacks an `account_id` or `user_id`, so we cannot prove the
    /// blob belongs to this seat. Nothing written to the slot; if the blob
    /// differs from what the slot holds it was preserved at `orphaned`.
    SkippedUnverifiable { orphaned: Option<PathBuf> },
    /// The source blob belongs to a different account/user. Nothing written to
    /// the slot; the blob was preserved at `orphaned` (if that succeeded).
    SkippedMismatch {
        expected: SeatIdentity,
        actual: SeatIdentity,
        orphaned: Option<PathBuf>,
    },
}

impl RefreshBackOutcome {
    /// True when the source blob was neither copied nor already present in
    /// the slot — i.e. the caller is about to overwrite something the side
    /// store does not hold. `orphaned` says whether it was preserved.
    pub fn is_skip(&self) -> bool {
        matches!(
            self,
            Self::SkippedUnparseable { .. }
                | Self::SkippedUnverifiable { .. }
                | Self::SkippedMismatch { .. }
        )
    }
}

/// Copy `src` into `seats/<name>/auth.json` **only** if the blob's identity
/// matches `expected`. The source is read once; the identity is derived from
/// that buffer and those exact bytes are written, so there is no window in
/// which a different file could be checked than the one copied.
///
/// A blob that cannot be copied is never lost: mismatched, unparseable, and
/// unverifiable-but-different blobs are parked under `orphaned/` because the
/// caller's next step is usually to overwrite the source.
pub fn refresh_back_from_guarded(
    src: &Path,
    name: &str,
    expected: &SeatIdentity,
) -> Result<RefreshBackOutcome> {
    let src_bytes = match fs::read(src) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RefreshBackOutcome::SkippedNoSource)
        }
        Err(e) => return Err(anyhow::Error::from(e).context(format!("reading {}", src.display()))),
    };
    let dst = seat_auth_path(name)?;
    let dst_bytes = fs::read(&dst).ok();
    let already_in_slot = dst_bytes.as_deref() == Some(src_bytes.as_slice());

    let actual = match read_identity(&src_bytes) {
        Ok(id) => id,
        Err(_) => {
            let orphaned = if already_in_slot { None } else { park_orphaned_blob(&src_bytes).ok() };
            return Ok(RefreshBackOutcome::SkippedUnparseable { orphaned });
        }
    };
    match expected.matches(&actual) {
        IdentityMatch::Unverifiable => {
            let orphaned = if already_in_slot { None } else { park_orphaned_blob(&src_bytes).ok() };
            return Ok(RefreshBackOutcome::SkippedUnverifiable { orphaned });
        }
        IdentityMatch::Mismatch => {
            let orphaned = park_orphaned_blob(&src_bytes).ok();
            return Ok(RefreshBackOutcome::SkippedMismatch {
                expected: expected.clone(),
                actual,
                orphaned,
            });
        }
        IdentityMatch::Match => {}
    }
    if already_in_slot {
        return Ok(RefreshBackOutcome::Unchanged);
    }
    atomic_write(&dst, &src_bytes)?;
    Ok(RefreshBackOutcome::Copied)
}

/// `refresh_back_from_guarded` with `~/.codex/auth.json` as the source.
pub fn refresh_back_guarded(name: &str, expected: &SeatIdentity) -> Result<RefreshBackOutcome> {
    refresh_back_from_guarded(&codex_auth_path()?, name, expected)
}

/// Preserve a blob that failed the identity guard so a real login is never
/// silently destroyed. Written 0600 under a 0700 `orphaned/`.
fn park_orphaned_blob(bytes: &[u8]) -> Result<PathBuf> {
    let dir = orphaned_dir()?;
    secure_create_dir_all(&dir)?;
    ensure_private_dir(&dir)?;
    let path = dir.join(format!(
        "auth-{}-{}.json",
        Utc::now().format("%Y%m%dT%H%M%S%.3fZ"),
        std::process::id()
    ));
    atomic_write(&path, bytes)?;
    Ok(path)
}

/// A directory that holds credentials must be a real directory (not a
/// symlink) owned by us with no group/other access, even if it pre-existed.
/// `secure_create_dir_all` deliberately leaves pre-existing directories
/// alone; this tightens the one leaf we are about to write secrets into.
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(dir)
        .with_context(|| format!("inspecting {}", dir.display()))?;
    if meta.file_type().is_symlink() {
        anyhow::bail!("{} is a symlink; refusing to write credentials through it", dir.display());
    }
    if !meta.is_dir() {
        anyhow::bail!("{} is not a directory", dir.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            let mut perms = meta.permissions();
            perms.set_mode(0o700);
            fs::set_permissions(dir, perms)
                .with_context(|| format!("tightening permissions on {}", dir.display()))?;
        }
    }
    Ok(())
}

/// Print a one-line warning for the non-success outcomes of a refresh-back.
pub fn warn_refresh_back(seat: &str, outcome: &RefreshBackOutcome) {
    fn preserved(orphaned: &Option<PathBuf>) {
        match orphaned {
            Some(p) => eprintln!("         The blob was preserved at {}.", p.display()),
            None => eprintln!(
                "         Warning: the blob could not be preserved under orphaned/; if it was a \
                 login you need, re-run `codex login`."
            ),
        }
    }
    match outcome {
        RefreshBackOutcome::Copied
        | RefreshBackOutcome::Unchanged
        | RefreshBackOutcome::SkippedNoSource => {}
        RefreshBackOutcome::SkippedUnparseable { orphaned } => {
            eprintln!(
                "Warning: the active auth.json is not parseable; not copying it into seat '{}'.",
                seat
            );
            preserved(orphaned);
        }
        RefreshBackOutcome::SkippedUnverifiable { orphaned } => {
            eprintln!(
                "Warning: could not verify which account the active auth.json belongs to; \
                 not copying it into seat '{}'.",
                seat
            );
            if orphaned.is_some() {
                preserved(orphaned);
            }
        }
        RefreshBackOutcome::SkippedMismatch { expected, actual, orphaned } => {
            eprintln!(
                "Warning: active auth.json belongs to {} but seat '{}' is {}; not copying it into that seat's slot.",
                actual, seat, expected
            );
            preserved(orphaned);
        }
    }
    match outcome {
        RefreshBackOutcome::SkippedMismatch { actual, orphaned, .. } => log_event(
            "refresh_back_mismatch",
            seat,
            &format!(
                "active auth.json is {}; parked={}",
                actual,
                orphaned.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "FAILED".into())
            ),
        ),
        RefreshBackOutcome::SkippedUnverifiable { orphaned: Some(p) }
        | RefreshBackOutcome::SkippedUnparseable { orphaned: Some(p) } => {
            log_event("refresh_back_skipped", seat, &format!("parked={}", p.display()))
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Identity extraction
// ---------------------------------------------------------------------------

/// Who an auth blob belongs to. `account_id` is the ChatGPT workspace;
/// `user_id` is the individual user within it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeatIdentity {
    pub account_id: Option<String>,
    pub user_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityMatch {
    Match,
    Mismatch,
    /// One side is missing `account_id` or `user_id`.
    Unverifiable,
}

impl SeatIdentity {
    pub fn matches(&self, other: &SeatIdentity) -> IdentityMatch {
        match (&self.account_id, &self.user_id, &other.account_id, &other.user_id) {
            (Some(a1), Some(u1), Some(a2), Some(u2)) => {
                if a1 == a2 && u1 == u2 {
                    IdentityMatch::Match
                } else {
                    IdentityMatch::Mismatch
                }
            }
            _ => IdentityMatch::Unverifiable,
        }
    }
}

impl std::fmt::Display for SeatIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "account '{}' / user '{}'",
            self.account_id.as_deref().unwrap_or("?"),
            self.user_id.as_deref().unwrap_or("?")
        )
    }
}

/// Extract the identity from the bytes of a codex auth.json. Missing fields
/// are `None` (older shapes, API-key logins); only a parse failure errors.
pub fn read_identity(auth_bytes: &[u8]) -> Result<SeatIdentity> {
    let v: serde_json::Value =
        serde_json::from_slice(auth_bytes).context("parsing auth.json")?;
    let account_id = v
        .pointer("/tokens/account_id")
        .and_then(|x| x.as_str())
        .map(String::from);
    let user_id = v
        .pointer("/tokens/id_token")
        .and_then(|x| x.as_str())
        .and_then(jwt_claims)
        .and_then(|claims| {
            claims
                .pointer("/https:~1~1api.openai.com~1auth/chatgpt_user_id")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| claims.get("sub").and_then(|x| x.as_str()).map(String::from))
        });
    Ok(SeatIdentity { account_id, user_id })
}

/// Read the identity from an auth.json path.
pub fn read_identity_from_path(auth_path: &Path) -> Result<SeatIdentity> {
    let bytes = fs::read(auth_path)
        .with_context(|| format!("reading {}", auth_path.display()))?;
    read_identity(&bytes).with_context(|| format!("parsing {}", auth_path.display()))
}

/// Extract `tokens.account_id` from a codex auth.json file. Returns `Ok(None)`
/// if the file is well-formed JSON but the field is missing (e.g. an older
/// auth.json shape, or an API-key login). Errors only on file/parse failure.
pub fn read_account_id(auth_path: &Path) -> Result<Option<String>> {
    Ok(read_identity_from_path(auth_path)?.account_id)
}

/// Decode the payload of a JWT without verifying it. We only need the claims
/// to tell users apart; codex itself is the party that trusts the token.
fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64url_decode(payload)?;
    serde_json::from_slice(&bytes).ok()
}

fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        })
    }
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// Build a fake-but-decodable auth.json for tests and fixtures: the id_token
/// is a structurally valid JWT whose payload carries `chatgpt_user_id`.
pub fn fake_auth_json_for_tests(account_id: &str, user_id: &str, token_tag: &str) -> String {
    let payload = format!(
        r#"{{"sub":"{user}","https://api.openai.com/auth":{{"chatgpt_account_id":"{aid}","chatgpt_user_id":"{user}"}}}}"#,
        user = user_id,
        aid = account_id
    );
    let id_token = format!("eyJhbGciOiJub25lIn0.{}.sig", base64url_encode(payload.as_bytes()));
    format!(
        r#"{{
  "auth_mode": "chatgpt",
  "tokens": {{
    "id_token": "{id_token}",
    "access_token": "fake-access-{tag}",
    "refresh_token": "fake-refresh-{tag}",
    "account_id": "{aid}"
  }},
  "last_refresh": "2026-04-28T12:00:00Z"
}}
"#,
        id_token = id_token,
        tag = token_tag,
        aid = account_id
    )
}

fn base64url_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut n: u32 = 0;
        for (i, b) in chunk.iter().enumerate() {
            n |= (*b as u32) << (16 - 8 * i);
        }
        let chars = match chunk.len() {
            1 => 2,
            2 => 3,
            _ => 4,
        };
        for i in 0..chars {
            out.push(TABLE[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Scratch CODEX_HOME (isolated home for login / status children)
// ---------------------------------------------------------------------------

/// An isolated `CODEX_HOME` under `seats/<name>.<purpose>-<pid>/`, removed on
/// drop. Used so a `codex login` or `codex app-server` child never touches
/// `~/.codex/auth.json`. Drop does not run on SIGKILL; see
/// [`scavenge_scratch_dirs`].
pub struct ScratchCodexHome {
    path: PathBuf,
}

impl ScratchCodexHome {
    pub fn create_for(name: &str, purpose: &str) -> Result<Self> {
        validate_seat_name(name)?;
        let path = seats_dir()?.join(format!("{}.{}-{}", name, purpose, std::process::id()));
        // If a previous run with this pid died and left this dir, blow it away.
        let _ = fs::remove_dir_all(&path);
        secure_create_dir_all(&path)?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn auth_path(&self) -> PathBuf {
        self.path.join("auth.json")
    }
}

impl Drop for ScratchCodexHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Seed `<home>/config.toml` so codex in that home reads/writes auth.json as a
/// file rather than the OS keyring. Without this the child's tokens would
/// never appear in the scratch home.
pub fn seed_file_store_config(home: &Path) -> Result<()> {
    let cfg_path = home.join("config.toml");
    fs::write(&cfg_path, "cli_auth_credentials_store = \"file\"\n")
        .with_context(|| format!("writing {}", cfg_path.display()))
}

/// Age after which a leftover scratch directory is assumed abandoned.
pub const SCRATCH_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Does `fname` have the exact scratch-home shape `<valid-name>.<purpose>-<pid>`?
/// Anything else (including a hand-made `work.uk` seat dir from before name
/// validation) is not ours to delete.
pub fn is_scratch_dir_name(fname: &str) -> bool {
    let Some((name, rest)) = fname.split_once('.') else { return false };
    if validate_seat_name(name).is_err() {
        return false;
    }
    let Some((purpose, pid)) = rest.split_once('-') else { return false };
    matches!(purpose, "partial" | "status")
        && !pid.is_empty()
        && pid.bytes().all(|b| b.is_ascii_digit())
}

/// Remove abandoned scratch homes (`seats/<name>.<purpose>-<pid>/`) older
/// than [`SCRATCH_STALE_AFTER`]. Returns the paths removed. Callers must hold
/// the `CodexLock`, so a live login/status in another process (which also
/// holds it) can never have its scratch home swept from under it.
pub fn scavenge_scratch_dirs() -> Result<Vec<PathBuf>> {
    let dir = seats_dir()?;
    let mut removed = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else { return Ok(removed) };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(fname) = path.file_name().and_then(|f| f.to_str()) else { continue };
        if !is_scratch_dir_name(fname) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&path) else { continue };
        if !meta.is_dir() {
            continue;
        }
        let age = meta.modified().ok().and_then(|m| now.duration_since(m).ok());
        if age.is_some_and(|a| a >= SCRATCH_STALE_AFTER) && fs::remove_dir_all(&path).is_ok() {
            removed.push(path);
        }
    }
    Ok(removed)
}

// ---------------------------------------------------------------------------
// Config patch: cli_auth_credentials_store = "file"
// ---------------------------------------------------------------------------

/// Outcome of inspecting / patching `~/.codex/config.toml` for the
/// `cli_auth_credentials_store = "file"` requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileStoreOutcome {
    /// Already correct; nothing changed.
    AlreadyFile,
    /// Was missing; we set it to "file".
    Added,
    /// Was set to a different value (typically "keyring"); we changed it
    /// to "file". The previous value is returned so callers can warn the
    /// user about keyring tokens becoming invisible.
    Changed { previous: String },
}

/// Ensure `~/.codex/config.toml` contains `cli_auth_credentials_store = "file"`.
/// Creates the file if it doesn't exist. Preserves all other keys / formatting
/// (best effort — round-trips through `toml::Value`).
pub fn ensure_file_credential_store() -> Result<FileStoreOutcome> {
    let path = codex_config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let raw = if path.exists() {
        let mut s = String::new();
        File::open(&path)?.read_to_string(&mut s)?;
        s
    } else {
        String::new()
    };

    let mut doc: toml::Value = if raw.trim().is_empty() {
        toml::Value::Table(toml::map::Map::new())
    } else {
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?
    };

    let table = doc
        .as_table_mut()
        .ok_or_else(|| anyhow!("{} top-level isn't a TOML table", path.display()))?;

    let outcome = match table.get("cli_auth_credentials_store") {
        None => FileStoreOutcome::Added,
        Some(toml::Value::String(s)) if s == "file" => FileStoreOutcome::AlreadyFile,
        Some(other) => FileStoreOutcome::Changed { previous: other.to_string().trim_matches('"').to_string() },
    };

    if outcome != FileStoreOutcome::AlreadyFile {
        table.insert(
            "cli_auth_credentials_store".to_string(),
            toml::Value::String("file".to_string()),
        );
        let new_raw = toml::to_string_pretty(&doc).context("serialising config.toml")?;
        atomic_write(&path, new_raw.as_bytes())?;
    }

    Ok(outcome)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn cfg(seats: &[&str], strategy: Strategy) -> SeatConfig {
        SeatConfig {
            seats: seats
                .iter()
                .map(|n| SeatEntry { name: n.to_string(), label: None, account_id: None, user_id: None })
                .collect(),
            rotation: RotationConfig { strategy, ..Default::default() },
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 28, 12, 0, 0).unwrap()
    }

    #[test]
    fn pick_seat_no_seats_returns_error() {
        let c = SeatConfig { seats: vec![], rotation: Default::default() };
        let s = SeatState::default();
        assert_eq!(pick_seat(&c, &s, None, now()), Err(SeatPickError::NoSeatsConfigured));
    }

    #[test]
    fn pick_seat_lru_picks_oldest() {
        let c = cfg(&["a", "b"], Strategy::LeastRecentlyUsed);
        let mut s = SeatState::default();
        s.entry_mut("a").last_used = Some(now() - chrono::Duration::hours(1));
        s.entry_mut("b").last_used = Some(now() - chrono::Duration::hours(2));
        assert_eq!(pick_seat(&c, &s, None, now()).unwrap(), "b");
    }

    #[test]
    fn pick_seat_lru_never_used_wins() {
        let c = cfg(&["a", "b"], Strategy::LeastRecentlyUsed);
        let mut s = SeatState::default();
        s.entry_mut("a").last_used = Some(now() - chrono::Duration::hours(1));
        // "b" has no last_used — should be picked as oldest.
        assert_eq!(pick_seat(&c, &s, None, now()).unwrap(), "b");
    }

    #[test]
    fn pick_seat_skips_cooling() {
        let c = cfg(&["a", "b"], Strategy::LeastRecentlyUsed);
        let mut s = SeatState::default();
        s.entry_mut("a").last_used = Some(now() - chrono::Duration::hours(2));
        s.entry_mut("a").cooldown_until = Some(now() + chrono::Duration::minutes(30));
        s.entry_mut("b").last_used = Some(now() - chrono::Duration::hours(1));
        // a is older but cooling → pick b.
        assert_eq!(pick_seat(&c, &s, None, now()).unwrap(), "b");
    }

    #[test]
    fn pick_seat_skips_needs_login() {
        let c = cfg(&["a", "b"], Strategy::LeastRecentlyUsed);
        let mut s = SeatState::default();
        s.entry_mut("a").last_used = Some(now() - chrono::Duration::hours(2));
        s.entry_mut("a").needs_login = true;
        s.entry_mut("b").last_used = Some(now() - chrono::Duration::hours(1));
        assert_eq!(pick_seat(&c, &s, None, now()).unwrap(), "b");
    }

    #[test]
    fn pick_seat_all_cooling_returns_soonest() {
        let c = cfg(&["a", "b"], Strategy::LeastRecentlyUsed);
        let mut s = SeatState::default();
        let later = now() + chrono::Duration::minutes(45);
        let sooner = now() + chrono::Duration::minutes(15);
        s.entry_mut("a").cooldown_until = Some(later);
        s.entry_mut("b").cooldown_until = Some(sooner);
        let err = pick_seat(&c, &s, None, now()).unwrap_err();
        assert_eq!(
            err,
            SeatPickError::AllSeatsBlocked {
                soonest_name: Some("b".to_string()),
                soonest_until: Some(sooner),
                cooling: 2,
                needs_login: 0,
            }
        );
    }

    #[test]
    fn pick_seat_all_needs_login_reports_zero_cooling() {
        let c = cfg(&["a", "b"], Strategy::LeastRecentlyUsed);
        let mut s = SeatState::default();
        s.entry_mut("a").needs_login = true;
        s.entry_mut("b").needs_login = true;
        let err = pick_seat(&c, &s, None, now()).unwrap_err();
        assert_eq!(
            err,
            SeatPickError::AllSeatsBlocked {
                soonest_name: None,
                soonest_until: None,
                cooling: 0,
                needs_login: 2,
            }
        );
        assert!(err.to_string().contains("need login"));
    }

    #[test]
    fn pick_seat_mixed_cooling_and_needs_login_counts_both() {
        let c = cfg(&["a", "b", "c"], Strategy::LeastRecentlyUsed);
        let mut s = SeatState::default();
        let until = now() + chrono::Duration::minutes(10);
        s.entry_mut("a").needs_login = true;
        s.entry_mut("b").cooldown_until = Some(until);
        s.entry_mut("c").cooldown_until = Some(until + chrono::Duration::minutes(5));
        let err = pick_seat(&c, &s, None, now()).unwrap_err();
        assert_eq!(
            err,
            SeatPickError::AllSeatsBlocked {
                soonest_name: Some("b".to_string()),
                soonest_until: Some(until),
                cooling: 2,
                needs_login: 1,
            }
        );
    }

    #[test]
    fn seat_config_validate_rejects_bad_and_duplicate_names() {
        let mut c = cfg(&["ok", "with/slash"], Strategy::LeastRecentlyUsed);
        assert!(c.validate().unwrap_err().to_string().contains("invalid characters"));
        c = cfg(&["dup", "dup"], Strategy::LeastRecentlyUsed);
        assert!(c.validate().unwrap_err().to_string().contains("duplicate"));
        // Case-folded duplicates collide on case-insensitive filesystems.
        c = cfg(&["Work", "work"], Strategy::LeastRecentlyUsed);
        assert!(c.validate().unwrap_err().to_string().contains("case-insensitively"));
        assert!(c.find_case_insensitive("WORK").is_some());
        c = cfg(&["a", "b"], Strategy::LeastRecentlyUsed);
        assert!(c.validate().is_ok());
    }

    fn snapshot_with(used: &[(u64, u32)], age_secs: i64) -> UsageSnapshot {
        UsageSnapshot {
            fetched_at: now() - chrono::Duration::seconds(age_secs),
            plan_type: Some("team".into()),
            buckets: vec![UsageBucket {
                limit_id: Some("codex".into()),
                limit_name: None,
                windows: used
                    .iter()
                    .map(|(m, p)| UsageWindow { window_minutes: Some(*m), used_percent: *p, resets_at: None })
                    .collect(),
                rate_limit_reached_type: None,
            }],
            credits: None,
            spend_control_reached: None,
        }
    }

    #[test]
    fn pick_seat_fixed_prefers_named_seat_and_falls_back_when_ineligible() {
        let mut c = cfg(&["main", "backup1"], Strategy::Fixed);
        c.rotation.fixed_seat = Some("backup1".into());
        let mut s = SeatState::default();
        // backup1 was used more recently than main; LRU would say main.
        s.entry_mut("backup1").last_used = Some(now() - chrono::Duration::minutes(1));
        s.entry_mut("main").last_used = Some(now() - chrono::Duration::hours(5));
        assert_eq!(pick_seat(&c, &s, None, now()).unwrap(), "backup1");
        // Preferred seat cooling → falls back to the other.
        s.entry_mut("backup1").cooldown_until = Some(now() + chrono::Duration::hours(1));
        assert_eq!(pick_seat(&c, &s, None, now()).unwrap(), "main");
        // Preferred seat needs login → same.
        s.entry_mut("backup1").cooldown_until = None;
        s.entry_mut("backup1").needs_login = true;
        assert_eq!(pick_seat(&c, &s, None, now()).unwrap(), "main");
    }

    #[test]
    fn fixed_strategy_requires_a_configured_fixed_seat() {
        let mut c = cfg(&["a", "b"], Strategy::Fixed);
        assert!(c.validate().unwrap_err().to_string().contains("requires rotation.fixed_seat"));
        c.rotation.fixed_seat = Some("nope".into());
        assert!(c.validate().unwrap_err().to_string().contains("not a configured seat"));
        c.rotation.fixed_seat = Some("b".into());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn pick_seat_balanced_prefers_most_headroom_on_tightest_window() {
        let c = cfg(&["main", "backup1"], Strategy::Balanced);
        let mut s = SeatState::default();
        // main: 5h 20% but weekly 86% → score 86. backup1: 5h 60%, weekly 4% → 60.
        s.entry_mut("main").usage = Some(snapshot_with(&[(300, 20), (10080, 86)], 60));
        s.entry_mut("backup1").usage = Some(snapshot_with(&[(300, 60), (10080, 4)], 60));
        // main was used longer ago; LRU would pick main. Balanced picks backup1.
        s.entry_mut("main").last_used = Some(now() - chrono::Duration::hours(3));
        s.entry_mut("backup1").last_used = Some(now() - chrono::Duration::minutes(1));
        assert_eq!(pick_seat(&c, &s, None, now()).unwrap(), "backup1");

        // Once backup1 catches up past main, main is picked again.
        s.entry_mut("backup1").usage = Some(snapshot_with(&[(300, 90), (10080, 40)], 60));
        assert_eq!(pick_seat(&c, &s, None, now()).unwrap(), "main");

        // Equal scores → LRU tie-break (main is older).
        s.entry_mut("backup1").usage = Some(snapshot_with(&[(300, 86)], 60));
        assert_eq!(pick_seat(&c, &s, None, now()).unwrap(), "main");

        // A seat with no snapshot counts as unused and is preferred.
        s.entry_mut("backup1").usage = None;
        assert_eq!(pick_seat(&c, &s, None, now()).unwrap(), "backup1");
        assert!(s.get("backup1").usage_is_stale(now(), chrono::Duration::seconds(1800)));
        assert!(!s.get("main").usage_is_stale(now(), chrono::Duration::seconds(1800)));
        assert!(s.get("main").usage_is_stale(now(), chrono::Duration::seconds(30)));
    }

    #[test]
    fn usage_score_ignores_non_enforced_buckets() {
        let mut st = SeatRuntimeState::default();
        let mut snap = snapshot_with(&[(300, 30), (10080, 70)], 0);
        snap.buckets[0].limit_id = Some("premium".into());
        st.usage = Some(snap);
        assert_eq!(st.usage_score(), None, "premium-only snapshot says nothing about headroom");
        let mut snap = snapshot_with(&[(300, 30), (10080, 70)], 0);
        snap.buckets[0].limit_id = None;
        st.usage = Some(snap);
        assert_eq!(st.usage_score(), Some(70), "legacy single view is enforced");
    }

    #[test]
    fn pick_seat_excluding_skips_tried_seats_for_every_strategy() {
        for strategy in [Strategy::LeastRecentlyUsed, Strategy::RoundRobin, Strategy::Fixed, Strategy::Balanced] {
            let mut c = cfg(&["a", "b"], strategy);
            c.rotation.fixed_seat = Some("a".into());
            let mut s = SeatState { active_seat: Some("b".into()), ..Default::default() };
            s.entry_mut("a").usage = Some(snapshot_with(&[(300, 1)], 0));
            s.entry_mut("b").usage = Some(snapshot_with(&[(300, 50)], 0));
            s.entry_mut("b").last_used = Some(now());
            // Every strategy would pick "a" first…
            assert_eq!(pick_seat(&c, &s, None, now()).unwrap(), "a", "{:?}", strategy);
            // …and "b" once "a" has been tried this run.
            assert_eq!(
                pick_seat_excluding(&c, &s, None, now(), &["a".to_string()]).unwrap(),
                "b",
                "{:?}",
                strategy
            );
            // With everything tried, exclusion is ignored (caller's guard handles it).
            assert_eq!(
                pick_seat_excluding(&c, &s, None, now(), &["a".to_string(), "b".to_string()]).unwrap(),
                "a",
                "{:?}",
                strategy
            );
        }
    }

    #[test]
    fn strategy_names_round_trip() {
        for s in [Strategy::LeastRecentlyUsed, Strategy::RoundRobin, Strategy::Fixed, Strategy::Balanced] {
            assert_eq!(Strategy::parse(s.as_str()), Some(s));
        }
        assert_eq!(Strategy::parse("LRU"), Some(Strategy::LeastRecentlyUsed));
        assert_eq!(Strategy::parse("rr"), Some(Strategy::RoundRobin));
        assert_eq!(Strategy::parse("weighted"), None);
        let raw = "[[seat]]\nname = \"a\"\n[rotation]\nstrategy = \"balanced\"\n";
        let c: SeatConfig = toml::from_str(raw).unwrap();
        assert_eq!(c.rotation.strategy, Strategy::Balanced);
        assert_eq!(c.rotation.balance_refresh_seconds, 1800);
    }

    #[test]
    fn seat_notice_summarises_degraded_pool_only() {
        let c = cfg(&["main", "backup1"], Strategy::LeastRecentlyUsed);
        let mut s = SeatState::default();
        assert_eq!(seat_notice(&c, &s, now()), None, "healthy pool is silent");

        s.entry_mut("backup1").needs_login = true;
        let n = seat_notice(&c, &s, now()).unwrap();
        assert!(n.contains("backup1 needs login (run: codex-clean seat login backup1)"), "{}", n);
        assert!(n.ends_with("1 of 2 usable"), "{}", n);

        s.entry_mut("main").cooldown_until = Some(now() + chrono::Duration::hours(1));
        s.entry_mut("main").cooldown_reason = Some("credits".into());
        let n = seat_notice(&c, &s, now()).unwrap();
        assert!(n.contains("main cooling until"), "{}", n);
        assert!(n.contains(", credits"), "{}", n);
        assert!(n.ends_with("0 of 2 usable"), "{}", n);

        // An expired cooldown is not degraded.
        s.entry_mut("main").cooldown_until = Some(now() - chrono::Duration::hours(1));
        let n = seat_notice(&c, &s, now()).unwrap();
        assert!(!n.contains("main"), "{}", n);
    }

    #[test]
    fn cool_seats_is_extend_only_and_workspace_siblings_share_account() {
        let mut c = cfg(&["main", "backup1", "other"], Strategy::LeastRecentlyUsed);
        c.seats[0].account_id = Some("ws-1".into());
        c.seats[1].account_id = Some("ws-1".into());
        c.seats[2].account_id = Some("ws-2".into());
        assert_eq!(workspace_siblings(&c, "main"), vec!["main", "backup1"]);
        assert_eq!(workspace_siblings(&c, "other"), vec!["other"]);
        c.seats[2].account_id = None;
        assert_eq!(workspace_siblings(&c, "other"), vec!["other"]);

        let mut s = SeatState::default();
        let long = now() + chrono::Duration::hours(20);
        s.entry_mut("backup1").cooldown_until = Some(long);
        s.entry_mut("backup1").cooldown_reason = Some("rate_limit".into());
        let short = now() + chrono::Duration::hours(1);
        let changed = cool_seats(
            &mut s,
            &["main".to_string(), "backup1".to_string()],
            short,
            "credits",
            now(),
        );
        assert_eq!(changed, vec!["main"]);
        assert_eq!(s.get("main").cooldown_until, Some(short));
        assert_eq!(s.get("main").cooldown_reason.as_deref(), Some("credits"));
        assert_eq!(s.get("backup1").cooldown_until, Some(long), "longer cooldown kept");
        assert_eq!(s.get("backup1").cooldown_reason.as_deref(), Some("rate_limit"));
    }

    #[test]
    fn log_excerpt_caps_and_strips_controls() {
        assert_eq!(log_excerpt("a\u{1b}[31mb", 10), "a [31mb");
        assert_eq!(log_excerpt(&"x".repeat(50), 10), format!("{}…", "x".repeat(10)));
    }

    #[test]
    fn scratch_dir_name_grammar_is_exact() {
        assert!(is_scratch_dir_name("work.status-4242"));
        assert!(is_scratch_dir_name("a_b-c.partial-1"));
        // Real (legacy, hand-made) seat dirs and anything else are left alone.
        assert!(!is_scratch_dir_name("work"));
        assert!(!is_scratch_dir_name("work.uk"));
        assert!(!is_scratch_dir_name("work.status"));
        assert!(!is_scratch_dir_name("work.status-"));
        assert!(!is_scratch_dir_name("work.status-12x"));
        assert!(!is_scratch_dir_name("work.backup-12"));
        assert!(!is_scratch_dir_name("bad/name.status-12"));
        assert!(!is_scratch_dir_name(".status-12"));
    }

    #[test]
    fn read_identity_extracts_account_and_user() {
        let blob = fake_auth_json_for_tests("acc-1", "user-1", "t");
        let id = read_identity(blob.as_bytes()).unwrap();
        assert_eq!(id.account_id.as_deref(), Some("acc-1"));
        assert_eq!(id.user_id.as_deref(), Some("user-1"));
    }

    #[test]
    fn read_identity_falls_back_to_sub_and_tolerates_garbage() {
        // Payload with only `sub`.
        let payload = base64url_encode(br#"{"sub":"subject-9"}"#);
        let blob = format!(
            r#"{{"tokens":{{"id_token":"h.{}.s","account_id":"acc"}}}}"#,
            payload
        );
        let id = read_identity(blob.as_bytes()).unwrap();
        assert_eq!(id.user_id.as_deref(), Some("subject-9"));

        // Undecodable id_token → user_id None, no error.
        let blob = r#"{"tokens":{"id_token":"fake.jwt.token","account_id":"acc"}}"#;
        let id = read_identity(blob.as_bytes()).unwrap();
        assert_eq!(id.account_id.as_deref(), Some("acc"));
        assert!(id.user_id.is_none());

        // Not JSON at all → error.
        assert!(read_identity(b"nope").is_err());
    }

    #[test]
    fn identity_match_semantics() {
        let full = |a: &str, u: &str| SeatIdentity {
            account_id: Some(a.into()),
            user_id: Some(u.into()),
        };
        assert_eq!(full("a", "u").matches(&full("a", "u")), IdentityMatch::Match);
        // Same workspace, different user — the Team-plan case.
        assert_eq!(full("a", "u1").matches(&full("a", "u2")), IdentityMatch::Mismatch);
        assert_eq!(full("a", "u").matches(&full("b", "u")), IdentityMatch::Mismatch);
        let partial = SeatIdentity { account_id: Some("a".into()), user_id: None };
        assert_eq!(full("a", "u").matches(&partial), IdentityMatch::Unverifiable);
        assert_eq!(partial.matches(&full("a", "u")), IdentityMatch::Unverifiable);
    }

    #[test]
    fn base64url_round_trip() {
        for s in ["", "a", "ab", "abc", "abcd", "{\"x\":1}"] {
            let enc = base64url_encode(s.as_bytes());
            assert_eq!(base64url_decode(&enc).unwrap(), s.as_bytes());
        }
        // Padding is tolerated.
        assert_eq!(base64url_decode("YQ==").unwrap(), b"a");
        assert!(base64url_decode("!!").is_none());
    }

    #[test]
    fn seat_state_loads_legacy_json_without_new_fields() {
        let raw = r#"{"seats":{"a":{"last_used":"2026-09-06T22:36:12Z","needs_login":false,"consecutive_failures":0}},"active_seat":"a"}"#;
        let s: SeatState = serde_json::from_str(raw).unwrap();
        let a = s.get("a");
        assert!(a.usage.is_none());
        assert!(a.cooldown_reason.is_none());
        // And a snapshot round-trips.
        let mut s2 = s.clone();
        s2.entry_mut("a").usage = Some(UsageSnapshot {
            fetched_at: now(),
            plan_type: Some("team".into()),
            buckets: vec![UsageBucket {
                limit_id: Some("codex".into()),
                limit_name: None,
                windows: vec![UsageWindow { window_minutes: Some(300), used_percent: 42, resets_at: Some(now()) }],
                rate_limit_reached_type: None,
            }],
            credits: Some(UsageCredits { has_credits: false, unlimited: false }),
            spend_control_reached: Some(false),
        });
        s2.entry_mut("a").cooldown_reason = Some("credits".into());
        let raw2 = serde_json::to_string(&s2).unwrap();
        let back: SeatState = serde_json::from_str(&raw2).unwrap();
        assert_eq!(s2, back);
    }

    #[test]
    fn pick_seat_round_robin_advances() {
        let c = cfg(&["a", "b", "c"], Strategy::RoundRobin);
        let mut s = SeatState { active_seat: Some("a".to_string()), ..Default::default() };
        // All eligible (no cooldowns, no needs_login).
        assert_eq!(pick_seat(&c, &s, None, now()).unwrap(), "b");
        s.active_seat = Some("c".to_string());
        assert_eq!(pick_seat(&c, &s, None, now()).unwrap(), "a");
    }

    #[test]
    fn pick_seat_round_robin_skips_ineligible() {
        let c = cfg(&["a", "b", "c"], Strategy::RoundRobin);
        let mut s = SeatState { active_seat: Some("a".to_string()), ..Default::default() };
        s.entry_mut("b").cooldown_until = Some(now() + chrono::Duration::minutes(30));
        // After a, b is cooling → c.
        assert_eq!(pick_seat(&c, &s, None, now()).unwrap(), "c");
    }

    #[test]
    fn pick_seat_override_honoured() {
        let c = cfg(&["a", "b"], Strategy::LeastRecentlyUsed);
        let mut s = SeatState::default();
        // a is older, but caller forces b.
        s.entry_mut("a").last_used = Some(now() - chrono::Duration::hours(2));
        s.entry_mut("b").last_used = Some(now() - chrono::Duration::hours(1));
        assert_eq!(pick_seat(&c, &s, Some("b"), now()).unwrap(), "b");
    }

    #[test]
    fn pick_seat_override_unknown_seat() {
        let c = cfg(&["a", "b"], Strategy::LeastRecentlyUsed);
        let s = SeatState::default();
        let err = pick_seat(&c, &s, Some("nope"), now()).unwrap_err();
        assert_eq!(err, SeatPickError::SeatNotFound("nope".to_string()));
    }

    #[test]
    fn pick_seat_override_cooling_returns_seat_cooling() {
        let c = cfg(&["a", "b"], Strategy::LeastRecentlyUsed);
        let mut s = SeatState::default();
        let until = now() + chrono::Duration::minutes(30);
        s.entry_mut("a").cooldown_until = Some(until);
        let err = pick_seat(&c, &s, Some("a"), now()).unwrap_err();
        assert_eq!(err, SeatPickError::SeatCooling { name: "a".to_string(), until });
    }

    #[test]
    fn pick_seat_override_needs_login() {
        let c = cfg(&["a", "b"], Strategy::LeastRecentlyUsed);
        let mut s = SeatState::default();
        s.entry_mut("a").needs_login = true;
        let err = pick_seat(&c, &s, Some("a"), now()).unwrap_err();
        assert_eq!(err, SeatPickError::SeatNeedsLogin("a".to_string()));
    }

    #[test]
    fn seat_state_round_trips_via_json() {
        let mut s = SeatState { active_seat: Some("a".to_string()), ..Default::default() };
        s.entry_mut("a").last_used = Some(now());
        s.entry_mut("a").cooldown_until = Some(now() + chrono::Duration::hours(1));
        s.entry_mut("a").consecutive_failures = 2;
        s.entry_mut("b").needs_login = true;
        let raw = serde_json::to_string(&s).unwrap();
        let back: SeatState = serde_json::from_str(&raw).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn seat_config_round_trips_via_toml() {
        let c = SeatConfig {
            seats: vec![
                SeatEntry { name: "a".into(), label: Some("Personal".into()), account_id: None, user_id: None },
                SeatEntry { name: "b".into(), label: None, account_id: Some("acc".into()), user_id: Some("u".into()) },
            ],
            rotation: RotationConfig {
                strategy: Strategy::RoundRobin,
                fixed_seat: None,
                balance_refresh_seconds: 600,
                default_cooldown_seconds: 1800,
                max_retries: 2,
                cooldown_min_seconds: 60,
                cooldown_max_seconds: 7200,
                cooldown_jitter_seconds: 30,
            },
        };
        let raw = toml::to_string(&c).unwrap();
        let back: SeatConfig = toml::from_str(&raw).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn seat_config_loads_with_defaults_when_rotation_missing() {
        // User-authored seats.toml may omit [rotation] entirely.
        let raw = r#"
[[seat]]
name = "a"
"#;
        let c: SeatConfig = toml::from_str(raw).unwrap();
        assert_eq!(c.seats.len(), 1);
        assert_eq!(c.rotation, RotationConfig::default());
    }

    #[test]
    fn rotation_config_validate_rejects_min_above_max() {
        let bad = RotationConfig {
            cooldown_min_seconds: 1000,
            cooldown_max_seconds: 500,
            ..Default::default()
        };
        let err = bad.validate().unwrap_err().to_string();
        assert!(err.contains("cooldown_min_seconds"));
        assert!(err.contains("cooldown_max_seconds"));
    }

    #[test]
    fn rotation_config_validate_accepts_equal_bounds() {
        let ok = RotationConfig {
            cooldown_min_seconds: 600,
            cooldown_max_seconds: 600,
            ..Default::default()
        };
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn atomic_write_creates_and_overwrites() {
        let dir = tempdir();
        let p = dir.join("data.txt");
        atomic_write(&p, b"hello").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"hello");
        atomic_write(&p, b"world").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"world");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_sets_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
        let p = dir.join("secret.json");
        atomic_write(&p, b"{}").unwrap();
        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "auth files must not be group/world readable");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_tightens_existing_loose_perms() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
        let p = dir.join("preexisting.json");
        // Create with a loose mode first, then atomic-overwrite.
        fs::write(&p, b"old").unwrap();
        let mut perms = fs::metadata(&p).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&p, perms).unwrap();
        atomic_write(&p, b"new").unwrap();
        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn atomic_write_creates_parent_dirs() {
        let dir = tempdir();
        let p = dir.join("nested").join("deep").join("data.txt");
        atomic_write(&p, b"x").unwrap();
        assert!(p.exists());
    }

    #[cfg(unix)]
    #[test]
    fn secure_create_dir_all_does_not_chmod_pre_existing_ancestor() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
        // Mark the pre-existing tempdir as world-readable to prove we leave it alone.
        let mut perms = fs::metadata(&dir).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&dir, perms).unwrap();

        let nested = dir.join("a").join("b");
        secure_create_dir_all(&nested).unwrap();

        let outer_mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            outer_mode, 0o755,
            "pre-existing ancestor outside any newly-created chain must be left alone"
        );
        let inner_mode = fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
        assert_eq!(inner_mode, 0o700, "newly created dirs must be tightened");
        let middle_mode = fs::metadata(dir.join("a")).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            middle_mode, 0o700,
            "intermediate newly-created dir must also be tightened"
        );
    }

    #[cfg(unix)]
    #[test]
    fn secure_create_dir_all_is_no_op_when_path_already_exists() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
        let mut perms = fs::metadata(&dir).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&dir, perms).unwrap();

        // Path is the tempdir itself — already exists, so nothing should be tightened.
        secure_create_dir_all(&dir).unwrap();
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o755,
            "secure_create_dir_all must never chmod a pre-existing leaf path"
        );
    }

    fn tempdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "codex-clean-test-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }
}
