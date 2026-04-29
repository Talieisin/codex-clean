//! Integration tests for the seat-aware run orchestration in `runner.rs`.
//!
//! These tests do NOT spawn the real codex binary. Instead, they call
//! `runner::run_codex_with` directly with a mock attempt closure that
//! returns canned outcomes based on which seat the orchestrator just
//! swapped into `~/.codex/auth.json`. The side store and codex home are
//! redirected to temp directories via `CODEX_CLEAN_HOME` and `CODEX_HOME`
//! so the user's real OAuth state is never touched.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use codex_clean::output::CodexOutput;
use codex_clean::runner::{self, AttemptResult, Mode};
use codex_clean::seat::{
    self, RotationConfig, SeatConfig, SeatEntry, SeatState, Strategy,
};
use tempfile::TempDir;

/// Tests in this file mutate process-global env vars (CODEX_CLEAN_HOME,
/// CODEX_HOME, CODEX_CLEAN_SEAT). They must run sequentially.
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct TestEnv {
    _clean_home: TempDir,
    _codex_home: TempDir,
    clean_home_path: PathBuf,
    codex_home_path: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let clean = tempfile::tempdir().unwrap();
        let codex = tempfile::tempdir().unwrap();
        std::env::set_var("CODEX_CLEAN_HOME", clean.path());
        std::env::set_var("CODEX_HOME", codex.path());
        std::env::remove_var("CODEX_CLEAN_SEAT");
        // Seed a config.toml so ensure_file_credential_store finds it.
        fs::write(
            codex.path().join("config.toml"),
            "cli_auth_credentials_store = \"file\"\n",
        )
        .unwrap();
        Self {
            clean_home_path: clean.path().to_path_buf(),
            codex_home_path: codex.path().to_path_buf(),
            _clean_home: clean,
            _codex_home: codex,
        }
    }

    fn write_seat(&self, name: &str, account_id: &str) {
        let seat_dir = self.clean_home_path.join("seats").join(name);
        fs::create_dir_all(&seat_dir).unwrap();
        let auth = fake_auth_json(account_id);
        fs::write(seat_dir.join("auth.json"), auth).unwrap();
    }

    fn save_config(&self, cfg: &SeatConfig) {
        cfg.save().unwrap();
    }

    fn save_state(&self, state: &SeatState) {
        state.save().unwrap();
    }

    fn load_state(&self) -> SeatState {
        SeatState::load().unwrap()
    }

    fn active_auth_account_id(&self) -> Option<String> {
        seat::read_account_id(&self.codex_home_path.join("auth.json"))
            .unwrap()
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        std::env::remove_var("CODEX_CLEAN_HOME");
        std::env::remove_var("CODEX_HOME");
        std::env::remove_var("CODEX_CLEAN_SEAT");
    }
}

fn fake_auth_json(account_id: &str) -> String {
    format!(
        r#"{{
  "auth_mode": "chatgpt",
  "tokens": {{
    "id_token": "fake.jwt.token",
    "access_token": "fake-access-{aid}",
    "refresh_token": "fake-refresh-{aid}",
    "account_id": "{aid}"
  }},
  "last_refresh": "2026-04-28T12:00:00Z"
}}
"#,
        aid = account_id
    )
}

fn cfg_with_seats(seats: &[(&str, &str)]) -> SeatConfig {
    SeatConfig {
        seats: seats
            .iter()
            .map(|(name, aid)| SeatEntry {
                name: name.to_string(),
                label: None,
                account_id: Some(aid.to_string()),
            })
            .collect(),
        rotation: RotationConfig {
            strategy: Strategy::LeastRecentlyUsed,
            // Tight bounds so test cooldowns are tiny.
            cooldown_min_seconds: 60,
            cooldown_max_seconds: 7200,
            cooldown_jitter_seconds: 0,
            ..Default::default()
        },
    }
}

fn ok_attempt() -> AttemptResult {
    AttemptResult {
        output: CodexOutput::default(),
        stderr_buffer: Vec::new(),
        stderr_truncated: false,
        stderr_error: None,
        exit_code: 0,
        status_success: true,
        child_exit: 0,
    }
}

fn rate_limit_attempt() -> AttemptResult {
    let mut output = CodexOutput::default();
    output.errors.push(
        "You've hit your usage limit. Try again at 5:32 PM.".to_string(),
    );
    AttemptResult {
        output,
        stderr_buffer: Vec::new(),
        stderr_truncated: false,
        stderr_error: None,
        exit_code: 1,
        status_success: false,
        child_exit: 1,
    }
}

fn auth_error_attempt() -> AttemptResult {
    let mut output = CodexOutput::default();
    output.errors.push(
        "Your access token could not be refreshed because your refresh token has expired."
            .to_string(),
    );
    AttemptResult {
        output,
        stderr_buffer: Vec::new(),
        stderr_truncated: false,
        stderr_error: None,
        exit_code: 1,
        status_success: false,
        child_exit: 1,
    }
}

/// Build a mock attempt closure that returns a canned result based on which
/// seat is currently swapped into `~/.codex/auth.json`. The mapping is
/// keyed by the `account_id` field of the active auth.json — so each call
/// the orchestrator makes is observed AFTER the swap, returning the right
/// canned outcome for the seat the orchestrator just chose.
fn mock_attempt<F: Fn(&str) -> AttemptResult + 'static>(
    codex_home: &Path,
    by_account: F,
) -> impl Fn(&[String], &str, &Mode, bool) -> anyhow::Result<AttemptResult> {
    let codex_home = codex_home.to_path_buf();
    let calls = RefCell::new(0usize);
    move |_args, _prompt, _mode, _scrub| {
        *calls.borrow_mut() += 1;
        let auth = codex_home.join("auth.json");
        let aid = seat::read_account_id(&auth)?
            .unwrap_or_else(|| "unknown".to_string());
        Ok(by_account(&aid))
    }
}

#[test]
fn no_seats_falls_through_to_attempt() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = TestEnv::new();
    // No seats.toml written — backwards-compat path.
    let attempt = |_args: &[String], _prompt: &str, _mode: &Mode, _scrub: bool| Ok(ok_attempt());
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 0);
}

#[test]
fn rotation_picks_lru_seat_and_marks_last_used() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    env.write_seat("b", "acc-b");
    env.save_config(&cfg_with_seats(&[("a", "acc-a"), ("b", "acc-b")]));

    // Mark seat-a as recently used so seat-b is the LRU pick.
    let mut state = SeatState::default();
    state.entry_mut("a").last_used =
        Some(chrono::Utc::now() - chrono::Duration::hours(1));
    env.save_state(&state);

    let codex_home = env.codex_home_path.clone();
    let attempt = mock_attempt(&codex_home, |_aid| ok_attempt());
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 0);

    // The orchestrator should have swapped seat-b (LRU) into ~/.codex/auth.json.
    assert_eq!(env.active_auth_account_id().as_deref(), Some("acc-b"));

    let final_state = env.load_state();
    assert_eq!(final_state.active_seat.as_deref(), Some("b"));
    let b_state = final_state.seats.get("b").cloned().unwrap_or_default();
    assert!(b_state.last_used.is_some(), "b should have last_used updated");
    assert_eq!(b_state.consecutive_failures, 0);
    assert!(b_state.cooldown_until.is_none());
}

#[test]
fn rate_limit_cools_seat_and_retries_on_next() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    env.write_seat("b", "acc-b");
    env.save_config(&cfg_with_seats(&[("a", "acc-a"), ("b", "acc-b")]));

    // No prior usage — LRU picks 'a' (first in list).
    env.save_state(&SeatState::default());

    let codex_home = env.codex_home_path.clone();
    // a 429s, b succeeds.
    let attempt = mock_attempt(&codex_home, |aid| match aid {
        "acc-a" => rate_limit_attempt(),
        "acc-b" => ok_attempt(),
        _ => panic!("unexpected account_id {}", aid),
    });
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 0, "retry on b should succeed");

    let st = env.load_state();
    let a_state = st.seats.get("a").cloned().unwrap_or_default();
    let b_state = st.seats.get("b").cloned().unwrap_or_default();
    assert!(
        a_state.cooldown_until.is_some(),
        "seat a should be cooling after 429"
    );
    assert_eq!(a_state.consecutive_failures, 1);
    assert!(
        b_state.cooldown_until.is_none(),
        "seat b succeeded so should not be cooling"
    );
    assert_eq!(st.active_seat.as_deref(), Some("b"));
}

#[test]
fn auth_error_marks_needs_login_and_does_not_retry() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    env.write_seat("b", "acc-b");
    env.save_config(&cfg_with_seats(&[("a", "acc-a"), ("b", "acc-b")]));
    env.save_state(&SeatState::default());

    let codex_home = env.codex_home_path.clone();
    let attempt = mock_attempt(&codex_home, |aid| match aid {
        "acc-a" => auth_error_attempt(),
        _ => panic!("auth error should not trigger a retry on another seat (saw {})", aid),
    });
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    // Auth error path returns the attempt's exit code unchanged (1).
    assert_eq!(exit, 1);

    let st = env.load_state();
    let a_state = st.seats.get("a").cloned().unwrap_or_default();
    assert!(a_state.needs_login, "seat a should be marked needs_login");
    assert!(
        a_state.cooldown_until.is_none(),
        "auth error should not set a cooldown"
    );
}

#[test]
fn all_cooling_short_circuits_to_75() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    env.write_seat("b", "acc-b");
    env.save_config(&cfg_with_seats(&[("a", "acc-a"), ("b", "acc-b")]));

    let mut state = SeatState::default();
    let cool_until = chrono::Utc::now() + chrono::Duration::minutes(30);
    state.entry_mut("a").cooldown_until = Some(cool_until);
    state.entry_mut("b").cooldown_until = Some(cool_until);
    env.save_state(&state);

    let attempt = |_args: &[String], _prompt: &str, _mode: &Mode, _scrub: bool| -> anyhow::Result<AttemptResult> {
        panic!("attempt must NOT be called when all seats are cooling")
    };
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 75, "EX_TEMPFAIL when all seats cooling");
}

#[test]
fn explicit_seat_override_does_not_rotate_on_rate_limit() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    env.write_seat("b", "acc-b");
    env.save_config(&cfg_with_seats(&[("a", "acc-a"), ("b", "acc-b")]));
    env.save_state(&SeatState::default());

    std::env::set_var("CODEX_CLEAN_SEAT", "a");

    let codex_home = env.codex_home_path.clone();
    // a 429s. With override pinning, we should NOT try b.
    let attempt = mock_attempt(&codex_home, |aid| match aid {
        "acc-a" => rate_limit_attempt(),
        other => panic!("override pin should prevent fallback (saw {})", other),
    });
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 1);

    let st = env.load_state();
    assert!(st.seats.get("a").map(|s| s.cooldown_until.is_some()).unwrap_or(false));
}

#[test]
fn success_clears_consecutive_failures() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    env.save_config(&cfg_with_seats(&[("a", "acc-a")]));
    let mut state = SeatState::default();
    state.entry_mut("a").consecutive_failures = 3;
    env.save_state(&state);

    let codex_home = env.codex_home_path.clone();
    let attempt = mock_attempt(&codex_home, |_| ok_attempt());
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 0);

    let st = env.load_state();
    let a_state = st.seats.get("a").cloned().unwrap_or_default();
    assert_eq!(a_state.consecutive_failures, 0);
    assert!(a_state.cooldown_until.is_none());
}

#[test]
fn refresh_back_is_called_after_attempt() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    env.save_config(&cfg_with_seats(&[("a", "acc-a")]));
    env.save_state(&SeatState::default());

    let codex_home = env.codex_home_path.clone();
    let clean_home = env.clean_home_path.clone();
    // Mock attempt rewrites ~/.codex/auth.json to simulate a token refresh
    // mid-run. After the orchestrator's refresh-back, the seat's side
    // store should reflect that refresh.
    let attempt = move |_args: &[String], _prompt: &str, _mode: &Mode, _scrub: bool| -> anyhow::Result<AttemptResult> {
        let new_blob = fake_auth_json("acc-a-refreshed");
        fs::write(codex_home.join("auth.json"), new_blob)?;
        Ok(ok_attempt())
    };
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 0);

    let side_store = clean_home.join("seats/a/auth.json");
    let aid = seat::read_account_id(&side_store).unwrap();
    assert_eq!(
        aid.as_deref(),
        Some("acc-a-refreshed"),
        "refresh-back must propagate token rotation into the side store"
    );
}
