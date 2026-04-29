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

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("could not resolve home directory"))
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RotationConfig {
    #[serde(default)]
    pub strategy: Strategy,
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
    #[default]
    LeastRecentlyUsed,
    RoundRobin,
}

fn default_default_cooldown() -> u64 { 3600 }
fn default_max_retries() -> u32 { 1 }
fn default_min_cooldown() -> u64 { 300 }
fn default_max_cooldown() -> u64 { 86400 }
fn default_jitter() -> u64 { 120 }

impl Default for RotationConfig {
    fn default() -> Self {
        Self {
            strategy: Strategy::LeastRecentlyUsed,
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
        let cfg: SeatConfig = toml::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        cfg.rotation.validate()
            .with_context(|| format!("invalid configuration in {}", path.display()))?;
        Ok(Some(cfg))
    }

    pub fn save(&self) -> Result<()> {
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
    #[serde(default)]
    pub needs_login: bool,
    #[serde(default)]
    pub consecutive_failures: u32,
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

    // Belt-and-braces: ensure existing files take 0600 even if the rename
    // landed on an older file with looser perms.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perms = meta.permissions();
            if perms.mode() & 0o777 != 0o600 {
                perms.set_mode(0o600);
                let _ = fs::set_permissions(path, perms);
            }
        }
    }

    // fsync the parent directory so the rename is durable across crashes.
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// Create `path` (and parents) with restrictive permissions on Unix. Any
/// directories we create get mode 0700 so the seat private store isn't
/// world-readable on shared systems.
pub fn secure_create_dir_all(path: &Path) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("creating {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Walk the chain we just created and tighten perms on anything that
        // isn't already at most 0700. We don't want to change perms on
        // pre-existing dirs further up the tree (e.g. ~/.config) — that's
        // the user's decision — so only tighten dirs whose mode bits are
        // wider than 0700.
        let mut cur: Option<&Path> = Some(path);
        while let Some(p) = cur {
            if let Ok(meta) = fs::metadata(p) {
                let mode = meta.permissions().mode() & 0o777;
                if mode != 0 && mode != 0o700 && (mode & 0o077) != 0 {
                    let mut perms = meta.permissions();
                    perms.set_mode(0o700);
                    let _ = fs::set_permissions(p, perms);
                }
            }
            cur = p.parent().filter(|pp| !pp.as_os_str().is_empty());
            // Stop once we hit something outside our config tree to avoid
            // touching the user's home directory permissions.
            if let Some(home) = dirs::home_dir() {
                if cur.map(|c| c == home).unwrap_or(false) {
                    break;
                }
            }
        }
    }
    Ok(())
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
    /// Acquire the lock, blocking until it's available.
    pub fn acquire() -> Result<Self> {
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
        file.lock_exclusive()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(Self { file })
    }
}

impl Drop for CodexLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
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
    AllSeatsBlocked { soonest_name: Option<String>, soonest_until: Option<DateTime<Utc>> },
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
            Self::AllSeatsBlocked { soonest_name, soonest_until } => match (soonest_name, soonest_until) {
                (Some(n), Some(u)) => write!(
                    f,
                    "all seats cooling; soonest available at {} (seat '{}')",
                    u.with_timezone(&chrono::Local).format("%-I:%M %p"),
                    n
                ),
                _ => write!(f, "no seats are eligible (all cooling or need login)"),
            },
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
    let eligible: Vec<&SeatEntry> = config
        .seats
        .iter()
        .filter(|s| {
            let st = state.get(&s.name);
            !st.needs_login
                && st.cooldown_until.map_or(true, |u| u <= now)
        })
        .collect();

    if eligible.is_empty() {
        // Find soonest-cooling seat (ignoring needs_login seats — those need user action).
        let soonest = config
            .seats
            .iter()
            .filter_map(|s| {
                let st = state.get(&s.name);
                if st.needs_login {
                    None
                } else {
                    st.cooldown_until.map(|u| (s.name.clone(), u))
                }
            })
            .min_by_key(|(_, u)| *u);
        return Err(SeatPickError::AllSeatsBlocked {
            soonest_name: soonest.as_ref().map(|(n, _)| n.clone()),
            soonest_until: soonest.map(|(_, u)| u),
        });
    }

    let chosen = match config.rotation.strategy {
        Strategy::LeastRecentlyUsed => {
            // Smallest last_used (None sorts as oldest).
            eligible
                .into_iter()
                .min_by_key(|s| state.get(&s.name).last_used)
                .expect("eligible non-empty")
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

/// Copy `~/.codex/auth.json` back to `seats/<name>/auth.json`. Captures any
/// token refreshes codex performed during the run. Idempotent.
pub fn refresh_back(name: &str) -> Result<()> {
    let src = codex_auth_path()?;
    if !src.exists() {
        // Nothing to copy — codex either hasn't been run yet or auth was wiped.
        return Ok(());
    }
    let dst = seat_auth_path(name)?;
    let src_bytes = fs::read(&src)
        .with_context(|| format!("reading {}", src.display()))?;
    if let Ok(dst_bytes) = fs::read(&dst) {
        if dst_bytes == src_bytes {
            return Ok(());
        }
    }
    atomic_write(&dst, &src_bytes)
}

// ---------------------------------------------------------------------------
// Account-id extraction
// ---------------------------------------------------------------------------

/// Extract `tokens.account_id` from a codex auth.json file. Returns `Ok(None)`
/// if the file is well-formed JSON but the field is missing (e.g. an older
/// auth.json shape, or an API-key login). Errors only on file/parse failure.
pub fn read_account_id(auth_path: &Path) -> Result<Option<String>> {
    let bytes = fs::read(auth_path)
        .with_context(|| format!("reading {}", auth_path.display()))?;
    let v: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {}", auth_path.display()))?;
    Ok(v.pointer("/tokens/account_id")
        .and_then(|x| x.as_str())
        .map(String::from))
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
            seats: seats.iter().map(|n| SeatEntry { name: n.to_string(), label: None, account_id: None }).collect(),
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
            }
        );
    }

    #[test]
    fn pick_seat_round_robin_advances() {
        let c = cfg(&["a", "b", "c"], Strategy::RoundRobin);
        let mut s = SeatState::default();
        s.active_seat = Some("a".to_string());
        // All eligible (no cooldowns, no needs_login).
        assert_eq!(pick_seat(&c, &s, None, now()).unwrap(), "b");
        s.active_seat = Some("c".to_string());
        assert_eq!(pick_seat(&c, &s, None, now()).unwrap(), "a");
    }

    #[test]
    fn pick_seat_round_robin_skips_ineligible() {
        let c = cfg(&["a", "b", "c"], Strategy::RoundRobin);
        let mut s = SeatState::default();
        s.active_seat = Some("a".to_string());
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
        let mut s = SeatState::default();
        s.active_seat = Some("a".to_string());
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
                SeatEntry { name: "a".into(), label: Some("Personal".into()), account_id: None },
                SeatEntry { name: "b".into(), label: None, account_id: None },
            ],
            rotation: RotationConfig {
                strategy: Strategy::RoundRobin,
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
