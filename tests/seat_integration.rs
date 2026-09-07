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

/// A decodable fake auth blob: `account_id` as given, user id derived from
/// it, token tag = account id. Mirrors what real seats look like closely
/// enough for identity guards to work.
fn fake_auth_json(account_id: &str) -> String {
    seat::fake_auth_json_for_tests(account_id, &format!("user-{}", account_id), account_id)
}

/// Same identity as `fake_auth_json(account_id)` but a different token, to
/// simulate a refresh.
fn fake_auth_json_refreshed(account_id: &str, tag: &str) -> String {
    seat::fake_auth_json_for_tests(account_id, &format!("user-{}", account_id), tag)
}

fn cfg_with_seats(seats: &[(&str, &str)]) -> SeatConfig {
    SeatConfig {
        seats: seats
            .iter()
            .map(|(name, aid)| SeatEntry {
                name: name.to_string(),
                label: None,
                account_id: Some(aid.to_string()),
                user_id: Some(format!("user-{}", aid)),
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
    // mid-run (same identity, new tokens). After the orchestrator's
    // refresh-back, the seat's side store should reflect that refresh.
    let attempt = move |_args: &[String], _prompt: &str, _mode: &Mode, _scrub: bool| -> anyhow::Result<AttemptResult> {
        fs::write(codex_home.join("auth.json"), fake_auth_json_refreshed("acc-a", "refreshed"))?;
        Ok(ok_attempt())
    };
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 0);

    let side_store = fs::read_to_string(clean_home.join("seats/a/auth.json")).unwrap();
    assert!(
        side_store.contains("fake-access-refreshed"),
        "refresh-back must propagate token rotation into the side store"
    );
}

// ===========================================================================
// Rotation hardening: refresh-back before swap, exit codes
// ===========================================================================

fn cfg_with_seats_min_cooldown(seats: &[(&str, &str)], min_seconds: u64) -> SeatConfig {
    let mut cfg = cfg_with_seats(seats);
    cfg.rotation.cooldown_min_seconds = min_seconds;
    cfg
}

fn slot_contents(env: &TestEnv, seat: &str) -> String {
    fs::read_to_string(env.clean_home_path.join("seats").join(seat).join("auth.json")).unwrap()
}

#[test]
fn refresh_back_before_swap_persists_previous_seat_refresh() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    env.write_seat("b", "acc-b");
    env.save_config(&cfg_with_seats(&[("a", "acc-a"), ("b", "acc-b")]));

    // 'a' is active and was used recently → LRU will pick 'b'. Meanwhile a
    // plain `codex` session refreshed a's tokens in ~/.codex/auth.json.
    let mut state = SeatState { active_seat: Some("a".to_string()), ..Default::default() };
    state.entry_mut("a").last_used = Some(chrono::Utc::now());
    env.save_state(&state);
    fs::write(
        env.codex_home_path.join("auth.json"),
        fake_auth_json_refreshed("acc-a", "a-refreshed-by-plain-codex"),
    )
    .unwrap();

    let codex_home = env.codex_home_path.clone();
    let attempt = mock_attempt(&codex_home, |aid| {
        assert_eq!(aid, "acc-b", "LRU should have picked b");
        ok_attempt()
    });
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 0);

    assert!(
        slot_contents(&env, "a").contains("fake-access-a-refreshed-by-plain-codex"),
        "a's refresh must be stashed before b is swapped in"
    );
    assert!(slot_contents(&env, "b").contains("fake-access-acc-b"), "b untouched");
}

#[test]
fn refresh_back_before_swap_same_seat_repick() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    env.save_config(&cfg_with_seats(&[("a", "acc-a")]));
    let state = SeatState { active_seat: Some("a".to_string()), ..Default::default() };
    env.save_state(&state);
    // Single seat: the same seat is re-picked. The global blob is fresher.
    fs::write(
        env.codex_home_path.join("auth.json"),
        fake_auth_json_refreshed("acc-a", "fresher"),
    )
    .unwrap();

    let codex_home = env.codex_home_path.clone();
    let seen = std::rc::Rc::new(RefCell::new(String::new()));
    let seen2 = seen.clone();
    let attempt = move |_args: &[String], _prompt: &str, _mode: &Mode, _scrub: bool| -> anyhow::Result<AttemptResult> {
        *seen2.borrow_mut() = fs::read_to_string(codex_home.join("auth.json"))?;
        Ok(ok_attempt())
    };
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 0);
    assert!(
        seen.borrow().contains("fake-access-fresher"),
        "the run must see the fresher token, not the stale slot copy"
    );
    assert!(slot_contents(&env, "a").contains("fake-access-fresher"));
}

#[test]
fn refresh_back_before_swap_skips_on_user_mismatch_and_writes_orphan() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    // Two seats in the SAME workspace (same account_id), different users —
    // the Team-plan case. Config records distinct user ids.
    let mut cfg = cfg_with_seats(&[("a", "ws-1"), ("b", "ws-1")]);
    cfg.seats[0].user_id = Some("user-alice".into());
    cfg.seats[1].user_id = Some("user-bob".into());
    env.save_config(&cfg);
    let seat_dir = env.clean_home_path.join("seats");
    fs::create_dir_all(seat_dir.join("a")).unwrap();
    fs::create_dir_all(seat_dir.join("b")).unwrap();
    fs::write(
        seat_dir.join("a/auth.json"),
        seat::fake_auth_json_for_tests("ws-1", "user-alice", "alice"),
    )
    .unwrap();
    fs::write(
        seat_dir.join("b/auth.json"),
        seat::fake_auth_json_for_tests("ws-1", "user-bob", "bob"),
    )
    .unwrap();

    // 'a' is recorded active but ~/.codex/auth.json actually holds BOB's
    // login (someone ran `codex login` as bob in between).
    let mut state = SeatState { active_seat: Some("a".to_string()), ..Default::default() };
    state.entry_mut("a").last_used = Some(chrono::Utc::now());
    env.save_state(&state);
    fs::write(
        env.codex_home_path.join("auth.json"),
        seat::fake_auth_json_for_tests("ws-1", "user-bob", "bob-fresh"),
    )
    .unwrap();

    let attempt = |_args: &[String], _prompt: &str, _mode: &Mode, _scrub: bool| Ok(ok_attempt());
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 0);

    assert!(
        slot_contents(&env, "a").contains("fake-access-alice"),
        "bob's blob must NOT be filed under alice's seat"
    );
    let orphans: Vec<_> = fs::read_dir(env.clean_home_path.join("orphaned"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(orphans.len(), 1, "the foreign blob must be preserved, not destroyed");
    let orphan = fs::read_to_string(orphans[0].path()).unwrap();
    assert!(orphan.contains("fake-access-bob-fresh"));
}

#[test]
fn refresh_back_guarded_outcomes() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    let full = seat::SeatIdentity {
        account_id: Some("acc-a".into()),
        user_id: Some("user-acc-a".into()),
    };

    // No source file.
    assert_eq!(
        seat::refresh_back_guarded("a", &full).unwrap(),
        seat::RefreshBackOutcome::SkippedNoSource
    );

    // Identical bytes.
    fs::write(env.codex_home_path.join("auth.json"), fake_auth_json("acc-a")).unwrap();
    assert_eq!(
        seat::refresh_back_guarded("a", &full).unwrap(),
        seat::RefreshBackOutcome::Unchanged
    );

    // Refreshed, same identity → copied.
    fs::write(
        env.codex_home_path.join("auth.json"),
        fake_auth_json_refreshed("acc-a", "new"),
    )
    .unwrap();
    assert_eq!(
        seat::refresh_back_guarded("a", &full).unwrap(),
        seat::RefreshBackOutcome::Copied
    );
    assert!(slot_contents(&env, "a").contains("fake-access-new"));

    // Expected identity lacks a user claim → unverifiable, nothing written to
    // the slot, but the differing blob is parked so the caller's swap cannot
    // destroy it.
    let partial = seat::SeatIdentity { account_id: Some("acc-a".into()), user_id: None };
    fs::write(
        env.codex_home_path.join("auth.json"),
        fake_auth_json_refreshed("acc-a", "newer"),
    )
    .unwrap();
    match seat::refresh_back_guarded("a", &partial).unwrap() {
        seat::RefreshBackOutcome::SkippedUnverifiable { orphaned: Some(p) } => {
            assert!(fs::read_to_string(p).unwrap().contains("fake-access-newer"));
        }
        other => panic!("expected parked unverifiable, got {:?}", other),
    }
    assert!(slot_contents(&env, "a").contains("fake-access-new"), "slot unchanged");

    // Unverifiable but byte-identical to the slot → nothing to preserve.
    fs::write(env.codex_home_path.join("auth.json"), slot_contents(&env, "a")).unwrap();
    assert_eq!(
        seat::refresh_back_guarded("a", &partial).unwrap(),
        seat::RefreshBackOutcome::SkippedUnverifiable { orphaned: None }
    );

    // Source blob has an undecodable id_token → unverifiable, parked.
    fs::write(
        env.codex_home_path.join("auth.json"),
        r#"{"tokens":{"id_token":"nope","access_token":"x","account_id":"acc-a"}}"#,
    )
    .unwrap();
    assert!(matches!(
        seat::refresh_back_guarded("a", &full).unwrap(),
        seat::RefreshBackOutcome::SkippedUnverifiable { orphaned: Some(_) }
    ));

    // Not JSON at all → unparseable, parked, no error.
    fs::write(env.codex_home_path.join("auth.json"), "garbage").unwrap();
    match seat::refresh_back_guarded("a", &full).unwrap() {
        seat::RefreshBackOutcome::SkippedUnparseable { orphaned: Some(p) } => {
            assert_eq!(fs::read_to_string(p).unwrap(), "garbage");
        }
        other => panic!("expected parked unparseable, got {:?}", other),
    }
}

#[test]
fn api_key_global_blob_survives_a_rotation_swap() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    env.write_seat("b", "acc-b");
    env.save_config(&cfg_with_seats(&[("a", "acc-a"), ("b", "acc-b")]));
    let mut state = SeatState { active_seat: Some("a".to_string()), ..Default::default() };
    state.entry_mut("a").last_used = Some(chrono::Utc::now());
    env.save_state(&state);
    // The user ran `codex login --with-api-key` in between: no identity at all.
    let api_key_blob = r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-test-not-real","tokens":null}"#;
    fs::write(env.codex_home_path.join("auth.json"), api_key_blob).unwrap();

    let attempt = |_args: &[String], _prompt: &str, _mode: &Mode, _scrub: bool| Ok(ok_attempt());
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 0);

    assert!(slot_contents(&env, "a").contains("fake-access-acc-a"), "slot a untouched");
    let orphans: Vec<_> = fs::read_dir(env.clean_home_path.join("orphaned")).unwrap().flatten().collect();
    assert_eq!(orphans.len(), 1, "the API-key login must be preserved before the swap replaces it");
    assert_eq!(fs::read_to_string(orphans[0].path()).unwrap(), api_key_blob);
}

#[test]
fn all_cooling_mid_run_returns_75() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    env.write_seat("b", "acc-b");
    env.save_config(&cfg_with_seats(&[("a", "acc-a"), ("b", "acc-b")]));
    env.save_state(&SeatState::default());

    let codex_home = env.codex_home_path.clone();
    let attempt = mock_attempt(&codex_home, |_| rate_limit_attempt());
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 75, "both seats rate-limited within one run is EX_TEMPFAIL");

    let st = env.load_state();
    assert!(st.get("a").cooldown_until.is_some());
    assert!(st.get("b").cooldown_until.is_some());
    assert_eq!(st.get("a").cooldown_reason.as_deref(), Some("rate_limit"));
}

#[test]
fn zero_cooldown_still_returns_75_mid_run() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    env.write_seat("b", "acc-b");
    // min cooldown 0 and no parseable recovery time → cooldowns could be
    // effectively expired by the time the loop ends. The tried-seats rule
    // must still yield 75.
    let mut cfg = cfg_with_seats_min_cooldown(&[("a", "acc-a"), ("b", "acc-b")], 0);
    cfg.rotation.default_cooldown_seconds = 0;
    cfg.rotation.cooldown_max_seconds = 0;
    env.save_config(&cfg);
    env.save_state(&SeatState::default());

    let codex_home = env.codex_home_path.clone();
    let attempt = mock_attempt(&codex_home, |_| {
        let mut a = rate_limit_attempt();
        a.output.errors = vec!["You've hit your usage limit. Try again later.".to_string()];
        a
    });
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 75);
}

#[test]
fn three_seats_two_rate_limited_returns_child_exit() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    env.write_seat("b", "acc-b");
    env.write_seat("c", "acc-c");
    env.save_config(&cfg_with_seats(&[("a", "acc-a"), ("b", "acc-b"), ("c", "acc-c")]));
    env.save_state(&SeatState::default());

    let codex_home = env.codex_home_path.clone();
    // max_retries = 1 → only two attempts; c is never tried and stays eligible.
    let attempt = mock_attempt(&codex_home, |aid| match aid {
        "acc-c" => panic!("c must not be tried with max_retries = 1"),
        _ => rate_limit_attempt(),
    });
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 1, "an eligible seat remains, so this is not EX_TEMPFAIL");
    assert!(env.load_state().get("c").cooldown_until.is_none());
}

#[test]
fn all_needs_login_up_front_returns_1_not_75() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    env.write_seat("b", "acc-b");
    env.save_config(&cfg_with_seats(&[("a", "acc-a"), ("b", "acc-b")]));
    let mut state = SeatState::default();
    state.entry_mut("a").needs_login = true;
    state.entry_mut("b").needs_login = true;
    env.save_state(&state);

    let attempt = |_args: &[String], _prompt: &str, _mode: &Mode, _scrub: bool| -> anyhow::Result<AttemptResult> {
        panic!("attempt must NOT be called")
    };
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 1, "user action required is not a transient failure");
}

#[test]
fn mixed_cooling_and_needs_login_up_front_returns_75() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    env.write_seat("b", "acc-b");
    env.save_config(&cfg_with_seats(&[("a", "acc-a"), ("b", "acc-b")]));
    let mut state = SeatState::default();
    state.entry_mut("a").needs_login = true;
    state.entry_mut("b").cooldown_until = Some(chrono::Utc::now() + chrono::Duration::minutes(30));
    env.save_state(&state);

    let attempt = |_args: &[String], _prompt: &str, _mode: &Mode, _scrub: bool| -> anyhow::Result<AttemptResult> {
        panic!("attempt must NOT be called")
    };
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 75, "one seat will come back on its own");
}

#[test]
fn credits_prose_cools_for_default_and_only_the_same_workspace() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    // a and b are different workspaces here, so a credits failure on a must
    // not touch b, and b gets tried and succeeds.
    env.write_seat("a", "acc-a");
    env.write_seat("b", "acc-b");
    env.save_config(&cfg_with_seats(&[("a", "acc-a"), ("b", "acc-b")]));
    env.save_state(&SeatState::default());

    let codex_home = env.codex_home_path.clone();
    let before = chrono::Utc::now();
    let attempt = mock_attempt(&codex_home, |aid| match aid {
        "acc-a" => {
            let mut a = rate_limit_attempt();
            a.output.errors = vec![
                "Your workspace is out of credits. Ask your workspace owner to refill in order to continue."
                    .to_string(),
            ];
            a
        }
        _ => ok_attempt(),
    });
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 0, "b should have been tried and succeeded");

    let st = env.load_state();
    let a = st.get("a");
    assert_eq!(a.cooldown_reason.as_deref(), Some("credits"));
    // Credits cool for the *default* (3600s in the test cfg): the user can top
    // up and carry on, so a day-long lockout would be wrong.
    let secs = (a.cooldown_until.unwrap() - before).num_seconds();
    assert!((3500..=3660).contains(&secs), "expected ~3600s, got {}", secs);
    assert!(st.get("b").cooldown_until.is_none(), "different workspace untouched");

    // The event log recorded it.
    let log = fs::read_to_string(env.clean_home_path.join("seat-events.log")).unwrap();
    assert!(log.contains("rate_limit seat=a reason=credits"), "{}", log);
    assert!(log.contains("out of credits"), "{}", log);
}

#[test]
fn exhaustion_in_final_agent_message_is_detected_and_cools_whole_workspace() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    // Same Team workspace, two users — exactly the real layout. Codex 0.153
    // delivers "out of credits" as the final agent message, not an error.
    let mut cfg = cfg_with_seats(&[("main", "ws-1"), ("backup1", "ws-1")]);
    cfg.seats[0].user_id = Some("user-alice".into());
    cfg.seats[1].user_id = Some("user-bob".into());
    env.save_config(&cfg);
    let seat_dir = env.clean_home_path.join("seats");
    fs::create_dir_all(seat_dir.join("main")).unwrap();
    fs::create_dir_all(seat_dir.join("backup1")).unwrap();
    fs::write(seat_dir.join("main/auth.json"), seat::fake_auth_json_for_tests("ws-1", "user-alice", "a")).unwrap();
    fs::write(seat_dir.join("backup1/auth.json"), seat::fake_auth_json_for_tests("ws-1", "user-bob", "b")).unwrap();
    env.save_state(&SeatState::default());

    let calls = std::rc::Rc::new(RefCell::new(0usize));
    let calls2 = calls.clone();
    let attempt = move |_args: &[String], _prompt: &str, _mode: &Mode, _scrub: bool| -> anyhow::Result<AttemptResult> {
        *calls2.borrow_mut() += 1;
        let mut output = CodexOutput::default();
        output.messages.push("Your workspace is out of credits. Add credits to continue.".to_string());
        Ok(AttemptResult {
            output,
            stderr_buffer: b"Reading additional input from stdin...\n".to_vec(),
            stderr_truncated: false,
            stderr_error: None,
            exit_code: 1,
            status_success: false,
            child_exit: 1,
        })
    };
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();

    assert_eq!(*calls.borrow(), 1, "the second seat shares the workspace; do not burn an attempt on it");
    assert_eq!(exit, 75, "both seats now cooling → EX_TEMPFAIL");
    let st = env.load_state();
    for name in ["main", "backup1"] {
        let e = st.get(name);
        assert!(e.cooldown_until.is_some(), "{} should be cooling", name);
        assert_eq!(e.cooldown_reason.as_deref(), Some("credits"), "{}", name);
    }
    assert_eq!(st.get("main").consecutive_failures, 1);
    assert_eq!(st.get("backup1").consecutive_failures, 0, "only the seat that ran counts a failure");
    let log = fs::read_to_string(env.clean_home_path.join("seat-events.log")).unwrap();
    assert!(log.contains("affected=main,backup1"), "{}", log);
}

#[test]
fn successful_run_does_not_classify_prose_about_credits() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    env.save_config(&cfg_with_seats(&[("a", "acc-a")]));
    env.save_state(&SeatState::default());

    let attempt = |_args: &[String], _prompt: &str, _mode: &Mode, _scrub: bool| -> anyhow::Result<AttemptResult> {
        let mut ok = ok_attempt();
        ok.output.messages.push("If your workspace is out of credits, the wrapper cools every seat.".to_string());
        Ok(ok)
    };
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 0);
    assert!(env.load_state().get("a").cooldown_until.is_none());
}

#[test]
fn auth_error_is_logged_to_events_and_unmatched_log_records_messages() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    env.save_config(&cfg_with_seats(&[("a", "acc-a")]));
    env.save_state(&SeatState::default());

    let codex_home = env.codex_home_path.clone();
    let attempt = mock_attempt(&codex_home, |_| auth_error_attempt());
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 1);
    let log = fs::read_to_string(env.clean_home_path.join("seat-events.log")).unwrap();
    assert!(log.contains("auth_error seat=a marked needs_login"), "{}", log);

    // An unclassified failure records the parsed errors and last message,
    // not just stderr, so it can actually be diagnosed later.
    let mut st = SeatState::default();
    st.entry_mut("a").needs_login = false;
    env.save_state(&st);
    let attempt = |_args: &[String], _prompt: &str, _mode: &Mode, _scrub: bool| -> anyhow::Result<AttemptResult> {
        let mut output = CodexOutput::default();
        output.errors.push("invalid_request_error: bad reasoning effort".to_string());
        output.messages.push("Some final agent text".to_string());
        Ok(AttemptResult {
            output,
            stderr_buffer: b"stderr tail line\n".to_vec(),
            stderr_truncated: false,
            stderr_error: None,
            exit_code: 1,
            status_success: false,
            child_exit: 1,
        })
    };
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 1);
    let unmatched = fs::read_to_string(env.clean_home_path.join("unmatched.log")).unwrap();
    assert!(unmatched.contains("invalid_request_error: bad reasoning effort"), "{}", unmatched);
    assert!(unmatched.contains("Some final agent text"), "{}", unmatched);
    assert!(unmatched.contains("stderr tail line"), "{}", unmatched);
}

// ===========================================================================
// seat status
// ===========================================================================

use codex_clean::seat::{SeatEntry as SE, UsageSnapshot};
use codex_clean::usage::{self, UsageClient, UsageFetchError};

fn snapshot(used_5h: u32, used_weekly: u32) -> UsageSnapshot {
    let now = chrono::Utc::now();
    UsageSnapshot {
        fetched_at: now,
        plan_type: Some("team".into()),
        buckets: vec![seat::UsageBucket {
            limit_id: Some("codex".into()),
            limit_name: None,
            windows: vec![
                seat::UsageWindow {
                    window_minutes: Some(300),
                    used_percent: used_5h,
                    resets_at: Some(now + chrono::Duration::hours(2)),
                },
                seat::UsageWindow {
                    window_minutes: Some(10080),
                    used_percent: used_weekly,
                    resets_at: Some(now + chrono::Duration::days(3)),
                },
            ],
            rate_limit_reached_type: None,
        }],
        credits: Some(seat::UsageCredits { has_credits: false, unlimited: false }),
        spend_control_reached: Some(false),
    }
}

type FetchFn = Box<dyn Fn(&SE) -> Result<UsageSnapshot, UsageFetchError> + Sync>;

/// Fake client: returns a canned result per seat and optionally rewrites the
/// seat's slot (what a real fetch's refresh-back does when the app-server
/// rotates the token).
struct FakeClient {
    by_seat: FetchFn,
    rewrite_slot_tag: Option<String>,
}

impl UsageClient for FakeClient {
    fn fetch(&self, seat_entry: &SE) -> Result<UsageSnapshot, UsageFetchError> {
        if let Some(tag) = &self.rewrite_slot_tag {
            let aid = seat_entry.account_id.clone().unwrap();
            let path = seat::seat_auth_path(&seat_entry.name).unwrap();
            fs::write(&path, fake_auth_json_refreshed(&aid, tag)).unwrap();
        }
        (self.by_seat)(seat_entry)
    }
}

#[test]
fn status_records_snapshot_sets_cooldown_and_syncs_active_global_auth() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    env.write_seat("b", "acc-b");
    env.save_config(&cfg_with_seats(&[("a", "acc-a"), ("b", "acc-b")]));
    let state = SeatState { active_seat: Some("a".to_string()), ..Default::default() };
    env.save_state(&state);
    // Global blob is a's (stale copy).
    fs::write(env.codex_home_path.join("auth.json"), fake_auth_json("acc-a")).unwrap();

    let client = FakeClient {
        by_seat: Box::new(|s| {
            Ok(match s.name.as_str() {
                "a" => snapshot(42, 88),
                _ => snapshot(100, 60), // b: 5h window exhausted
            })
        }),
        rewrite_slot_tag: Some("rotated".into()),
    };
    let code = codex_clean::seat_cmd::status_with(&client, None, false, None).unwrap();
    assert_eq!(code, 0);

    let st = env.load_state();
    let a = st.get("a");
    let b = st.get("b");
    assert_eq!(a.usage.as_ref().unwrap().plan_type.as_deref(), Some("team"));
    assert!(a.cooldown_until.is_none(), "healthy seat not cooled");
    assert!(b.cooldown_until.is_some(), "exhausted seat cooled");
    assert_eq!(b.cooldown_reason.as_deref(), Some("rate_limit"));

    // The active seat's rotated token must have been pushed into ~/.codex/auth.json.
    let global = fs::read_to_string(env.codex_home_path.join("auth.json")).unwrap();
    assert!(global.contains("fake-access-rotated"), "global auth must follow the active slot");
    // Non-active seat's slot was rewritten too, but the global blob still belongs to 'a'.
    assert_eq!(env.active_auth_account_id().as_deref(), Some("acc-a"));
}

#[test]
fn status_syncs_plain_codex_refresh_into_active_slot_first() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    env.save_config(&cfg_with_seats(&[("a", "acc-a")]));
    let state = SeatState { active_seat: Some("a".to_string()), ..Default::default() };
    env.save_state(&state);
    // Plain codex refreshed the global blob since the last run.
    fs::write(
        env.codex_home_path.join("auth.json"),
        fake_auth_json_refreshed("acc-a", "plain-codex"),
    )
    .unwrap();

    let client = FakeClient {
        by_seat: Box::new(|_| Ok(snapshot(1, 2))),
        rewrite_slot_tag: None,
    };
    let code = codex_clean::seat_cmd::status_with(&client, None, true, None).unwrap();
    assert_eq!(code, 0);
    assert!(slot_contents(&env, "a").contains("fake-access-plain-codex"));
}

#[test]
fn status_clear_cooldown_and_never_clears_implicitly() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    env.save_config(&cfg_with_seats(&[("a", "acc-a")]));
    let mut state = SeatState::default();
    let until = chrono::Utc::now() + chrono::Duration::hours(1);
    state.entry_mut("a").cooldown_until = Some(until);
    state.entry_mut("a").needs_login = true;
    env.save_state(&state);

    let client = FakeClient {
        by_seat: Box::new(|_| Ok(snapshot(3, 4))),
        rewrite_slot_tag: None,
    };
    codex_clean::seat_cmd::status_with(&client, None, false, None).unwrap();
    let a = env.load_state().get("a");
    assert_eq!(a.cooldown_until, Some(until), "healthy read must not clear a cooldown");
    assert!(a.needs_login, "healthy read must not clear needs_login");

    codex_clean::seat_cmd::status_with(&client, None, false, Some("a")).unwrap();
    let a = env.load_state().get("a");
    assert!(a.cooldown_until.is_none(), "--clear-cooldown clears it");
    assert!(a.needs_login, "but never needs_login");
}

#[test]
fn status_all_failed_exits_1_and_busy_lock_errors() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    env.save_config(&cfg_with_seats(&[("a", "acc-a")]));
    env.save_state(&SeatState::default());

    let client = FakeClient {
        by_seat: Box::new(|_| Err(UsageFetchError::AuthRequired)),
        rewrite_slot_tag: None,
    };
    let code = codex_clean::seat_cmd::status_with(&client, None, false, None).unwrap();
    assert_eq!(code, 1);

    let _held = seat::CodexLock::acquire().unwrap();
    let err = codex_clean::seat_cmd::status_with(&client, None, false, None).unwrap_err();
    assert!(err.to_string().contains("in progress"), "{}", err);
}

#[test]
fn fetch_usage_with_refreshes_back_scratch_auth_and_cleans_up() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    let cfg = cfg_with_seats(&[("a", "acc-a")]);
    env.save_config(&cfg);

    let now = chrono::Utc::now();
    let seen_home = std::cell::Cell::new(None::<PathBuf>);
    let snap = usage::fetch_usage_with(&cfg.seats[0], now, |home| {
        seen_home.set(Some(home.to_path_buf()));
        assert!(home.join("auth.json").exists(), "slot blob staged into scratch home");
        assert!(
            fs::read_to_string(home.join("config.toml"))
                .unwrap()
                .contains("cli_auth_credentials_store = \"file\""),
            "scratch home must force the file credential store"
        );
        // Simulate the app-server rotating the token.
        fs::write(home.join("auth.json"), fake_auth_json_refreshed("acc-a", "app-server")).unwrap();
        Ok(serde_json::json!({"rateLimits": {"planType": "team",
            "primary": {"usedPercent": 7, "windowDurationMins": 300}}}))
    })
    .unwrap();
    assert_eq!(snap.plan_type.as_deref(), Some("team"));
    assert!(slot_contents(&env, "a").contains("fake-access-app-server"));
    assert!(!seen_home.take().unwrap().exists(), "scratch home removed");

    // A rotated blob with a DIFFERENT identity must not be filed into the slot.
    let _ = usage::fetch_usage_with(&cfg.seats[0], now, |home| {
        fs::write(home.join("auth.json"), seat::fake_auth_json_for_tests("acc-a", "user-someone-else", "x")).unwrap();
        Ok(serde_json::json!({"rateLimits": {}}))
    })
    .unwrap();
    assert!(slot_contents(&env, "a").contains("fake-access-app-server"), "slot unchanged");
}

#[test]
fn scavenge_removes_stale_scratch_dirs_only() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    let seats = env.clean_home_path.join("seats");
    let stale = seats.join("a.status-1234");
    let fresh = seats.join("a.partial-5678");
    fs::create_dir_all(&stale).unwrap();
    fs::create_dir_all(&fresh).unwrap();
    // Backdate the stale one by two hours.
    let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600);
    let f = fs::File::open(&stale).unwrap();
    f.set_modified(two_hours_ago).unwrap();

    // A legacy hand-made dotted seat dir is not scratch, however old.
    let dotted = seats.join("work.uk");
    fs::create_dir_all(&dotted).unwrap();
    fs::File::open(&dotted).unwrap().set_modified(two_hours_ago).unwrap();

    let removed = seat::scavenge_scratch_dirs().unwrap();
    assert_eq!(removed, vec![stale.clone()]);
    assert!(!stale.exists());
    assert!(fresh.exists(), "recent scratch dirs (possibly live) are left alone");
    assert!(seats.join("a").exists(), "real seat dirs are never touched");
    assert!(dotted.exists(), "only the exact scratch grammar is swept");
}

// ---------------------------------------------------------------------------
// End-to-end against a fake `codex` script on PATH (unix only)
// ---------------------------------------------------------------------------

#[cfg(unix)]
struct PathGuard {
    old: Option<std::ffi::OsString>,
}

#[cfg(unix)]
impl PathGuard {
    fn prepend(dir: &Path) -> Self {
        let old = std::env::var_os("PATH");
        let mut new = dir.as_os_str().to_os_string();
        if let Some(o) = &old {
            new.push(":");
            new.push(o);
        }
        std::env::set_var("PATH", new);
        Self { old }
    }
}

#[cfg(unix)]
impl Drop for PathGuard {
    fn drop(&mut self) {
        match &self.old {
            Some(o) => std::env::set_var("PATH", o),
            None => std::env::remove_var("PATH"),
        }
    }
}

#[cfg(unix)]
const CANNED_RATE_LIMITS: &str = r#"{"id":2,"result":{"rateLimits":{"limitId":"codex","planType":"team","primary":{"usedPercent":42,"windowDurationMins":300,"resetsAt":4102444800},"secondary":{"usedPercent":100,"windowDurationMins":10080,"resetsAt":4102448400},"credits":{"hasCredits":false,"unlimited":false},"rateLimitReachedType":null,"spendControlReached":false}}}"#;

/// Write an executable fake `codex` whose `app-server` mode is `preamble` +
/// a JSON-RPC loop with `on_read` as the body for `account/rateLimits/read`.
#[cfg(unix)]
fn install_fake_codex(dir: &Path, preamble: &str, on_read: &str) {
    use std::os::unix::fs::PermissionsExt;
    let script = format!(
        r#"#!/bin/bash
if [ "$1" != "app-server" ]; then echo "fake codex: unexpected args: $*" >&2; exit 2; fi
{preamble}
while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*) echo '{{"id":1,"result":{{"userAgent":"fake"}}}}' ;;
    *'"initialized"'*) echo '{{"method":"account/rateLimits/updated","params":{{}}}}' ;;
    *'rateLimits/read'*) {on_read} ;;
  esac
done
"#
    );
    let path = dir.join("codex");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
fn quick_client() -> usage::AppServerClient {
    usage::AppServerClient { timeout: std::time::Duration::from_secs(3) }
}

#[cfg(unix)]
#[test]
fn status_end_to_end_with_fake_codex() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    let cfg = cfg_with_seats(&[("a", "acc-a")]);
    env.save_config(&cfg);

    let bin = tempfile::tempdir().unwrap();
    let refreshed = bin.path().join("refreshed.json");
    fs::write(&refreshed, fake_auth_json_refreshed("acc-a", "from-app-server")).unwrap();
    // The fake writes the rotated token only AFTER answering (and after a
    // pause), so the slot can only pick it up if the client really waits for
    // the child to exit before reading the scratch auth.json.
    install_fake_codex(
        bin.path(),
        "",
        &format!(
            "echo '{}'; sleep 0.3; cp '{}' \"$CODEX_HOME/auth.json\"",
            CANNED_RATE_LIMITS,
            refreshed.display()
        ),
    );
    let _path = PathGuard::prepend(bin.path());

    let snap = quick_client().fetch(&cfg.seats[0]).unwrap();
    assert_eq!(snap.plan_type.as_deref(), Some("team"));
    let b = usage::primary_bucket(&snap).unwrap();
    assert_eq!(usage::find_window(b, 300).unwrap().used_percent, 42);
    assert_eq!(usage::find_window(b, 10080).unwrap().used_percent, 100);
    assert!(
        slot_contents(&env, "a").contains("fake-access-from-app-server"),
        "token rotated by the app-server lands in the slot"
    );
    assert!(
        matches!(usage::verdict(&snap), usage::UsageVerdict::Exhausted { .. }),
        "weekly window at 100% is exhaustion"
    );
    // No scratch dirs left behind.
    let leftovers: Vec<_> = fs::read_dir(env.clean_home_path.join("seats"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains('.'))
        .collect();
    assert!(leftovers.is_empty(), "scratch dirs must be removed: {:?}", leftovers);
}

#[cfg(unix)]
#[test]
fn fake_codex_error_variants() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    let cfg = cfg_with_seats(&[("a", "acc-a")]);
    env.save_config(&cfg);
    let bin = tempfile::tempdir().unwrap();
    let _path = PathGuard::prepend(bin.path());

    // Auth required.
    install_fake_codex(
        bin.path(),
        "",
        r#"echo '{"id":2,"error":{"code":-32600,"message":"chatgpt authentication required to read rate limits"}}'"#,
    );
    assert!(matches!(
        quick_client().fetch(&cfg.seats[0]),
        Err(UsageFetchError::AuthRequired)
    ));

    // Method not found (old codex).
    install_fake_codex(
        bin.path(),
        "",
        r#"echo '{"id":2,"error":{"code":-32601,"message":"Method not found"}}'"#,
    );
    assert!(matches!(
        quick_client().fetch(&cfg.seats[0]),
        Err(UsageFetchError::MethodNotFound)
    ));

    // Stdout closed early.
    install_fake_codex(bin.path(), "echo 'fake codex: giving up' >&2; exit 0", "true");
    match quick_client().fetch(&cfg.seats[0]) {
        Err(UsageFetchError::Protocol(m)) => {
            assert!(m.contains("closed its output"), "{}", m);
            assert!(!m.contains("giving up"), "child stderr must not leak into the error string: {}", m);
        }
        other => panic!("expected Protocol, got {:?}", other),
    }

    // A giant unterminated stdout frame is rejected promptly, not buffered.
    install_fake_codex(bin.path(), "head -c 3000000 /dev/zero | tr '\\0' 'x'; exec sleep 30", "true");
    let started = std::time::Instant::now();
    let res = quick_client().fetch(&cfg.seats[0]);
    assert!(matches!(res, Err(UsageFetchError::Protocol(_))), "{:?}", res);
    assert!(started.elapsed() < std::time::Duration::from_secs(8));

    // Stderr flood beyond the tail cap must not deadlock the client.
    install_fake_codex(
        bin.path(),
        "head -c 200000 /dev/zero | tr '\\0' 'e' >&2; echo >&2",
        &format!("echo '{}'", CANNED_RATE_LIMITS),
    );
    assert!(quick_client().fetch(&cfg.seats[0]).is_ok());
}

#[cfg(unix)]
#[test]
fn fake_codex_hang_times_out_and_child_is_reaped() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    let cfg = cfg_with_seats(&[("a", "acc-a")]);
    env.save_config(&cfg);
    let bin = tempfile::tempdir().unwrap();
    let pidfile = bin.path().join("pid");
    install_fake_codex(
        bin.path(),
        &format!("echo $$ > '{}'; exec sleep 60", pidfile.display()),
        "true",
    );
    let _path = PathGuard::prepend(bin.path());

    let started = std::time::Instant::now();
    let client = usage::AppServerClient { timeout: std::time::Duration::from_millis(500) };
    let res = client.fetch(&cfg.seats[0]);
    assert!(matches!(res, Err(UsageFetchError::Timeout(_))), "{:?}", res);
    // 500 ms request budget + at most SHUTDOWN_GRACE-bounded teardown.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(4),
        "teardown must stay within the budget, took {:?}",
        started.elapsed()
    );

    let pid = fs::read_to_string(&pidfile).unwrap().trim().to_string();
    let alive = std::process::Command::new("kill")
        .args(["-0", &pid])
        .status()
        .unwrap()
        .success();
    assert!(!alive, "hung app-server child must be killed and reaped");
}


#[test]
fn real_turn_failed_frames_classify_with_reason() {
    use codex_clean::ratelimit::{self, CooldownReason, FailureKind};
    use std::io::BufReader;
    // Frame shapes as codex exec --json emits them: prose `message` only.
    let cases = [
        (
            r#"{"type":"turn.failed","error":{"message":"You've hit your usage limit. To get more access now, send a request to your admin or try again at 5:32 PM."}}"#,
            CooldownReason::RateLimit,
        ),
        (
            r#"{"type":"turn.failed","error":{"message":"Your workspace is out of credits. Ask your workspace owner to refill in order to continue."}}"#,
            CooldownReason::Credits,
        ),
        (
            r#"{"type":"error","message":"You hit your spend cap set in your workspace. Increase your spend cap to continue."}"#,
            CooldownReason::SpendControl,
        ),
    ];
    for (frame, expected) in cases {
        let stream = format!("{{\"type\":\"thread.started\",\"thread_id\":\"t\"}}\n{}\n", frame);
        let out = runner::parse_codex_stream(BufReader::new(stream.as_bytes())).unwrap();
        assert_eq!(out.errors.len(), 1, "{}", frame);
        match ratelimit::classify(&out.errors) {
            FailureKind::RateLimit { reason, .. } => assert_eq!(reason, expected, "{}", frame),
            other => panic!("{} → {:?}", frame, other),
        }
    }
}

#[test]
fn verify_login_identity_requires_recorded_claims() {
    use codex_clean::seat_cmd::{verify_login_identity, LoginIdentityCheck};
    let id = |a: Option<&str>, u: Option<&str>| seat::SeatIdentity {
        account_id: a.map(String::from),
        user_id: u.map(String::from),
    };
    let full = id(Some("ws"), Some("alice"));
    assert_eq!(verify_login_identity(&full, &full), LoginIdentityCheck::Ok);
    // Same workspace, different colleague.
    assert_eq!(
        verify_login_identity(&full, &id(Some("ws"), Some("bob"))),
        LoginIdentityCheck::Mismatch
    );
    // Recorded user claim missing from the new blob → refused, not waved through.
    assert_eq!(
        verify_login_identity(&full, &id(Some("ws"), None)),
        LoginIdentityCheck::MissingClaims
    );
    // Mismatch outranks missing.
    assert_eq!(
        verify_login_identity(&full, &id(Some("other"), None)),
        LoginIdentityCheck::Mismatch
    );
    // Legacy seat that never recorded a user id adopts whatever comes.
    assert_eq!(
        verify_login_identity(&id(Some("ws"), None), &id(Some("ws"), Some("alice"))),
        LoginIdentityCheck::Ok
    );
    assert_eq!(verify_login_identity(&id(None, None), &id(None, None)), LoginIdentityCheck::Ok);
}

#[cfg(unix)]
#[test]
fn relogin_of_active_seat_updates_global_auth_and_survives_next_run() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    env.save_config(&cfg_with_seats(&[("a", "acc-a")]));
    let state = SeatState { active_seat: Some("a".to_string()), ..Default::default() };
    env.save_state(&state);
    fs::write(env.codex_home_path.join("auth.json"), fake_auth_json("acc-a")).unwrap();

    // Fake `codex login --device-auth` that writes a fresh blob for the same user.
    let bin = tempfile::tempdir().unwrap();
    let fresh = bin.path().join("fresh.json");
    fs::write(&fresh, fake_auth_json_refreshed("acc-a", "relogin")).unwrap();
    let script = format!(
        "#!/bin/bash\nif [ \"$1\" = login ]; then cp '{}' \"$CODEX_HOME/auth.json\"; exit 0; fi\necho unexpected >&2; exit 2\n",
        fresh.display()
    );
    let path = bin.path().join("codex");
    fs::write(&path, script).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let _path = PathGuard::prepend(bin.path());

    codex_clean::seat_cmd::login("a", false).unwrap();
    assert!(slot_contents(&env, "a").contains("fake-access-relogin"));
    let global = fs::read_to_string(env.codex_home_path.join("auth.json")).unwrap();
    assert!(
        global.contains("fake-access-relogin"),
        "re-login of the active seat must also update ~/.codex/auth.json"
    );

    // The next run must not roll the slot back to the pre-login tokens.
    let codex_home = env.codex_home_path.clone();
    let seen = std::rc::Rc::new(RefCell::new(String::new()));
    let seen2 = seen.clone();
    let attempt = move |_args: &[String], _prompt: &str, _mode: &Mode, _scrub: bool| -> anyhow::Result<AttemptResult> {
        *seen2.borrow_mut() = fs::read_to_string(codex_home.join("auth.json"))?;
        Ok(ok_attempt())
    };
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 0);
    assert!(seen.borrow().contains("fake-access-relogin"));
    assert!(slot_contents(&env, "a").contains("fake-access-relogin"));
}

#[test]
fn status_clear_cooldown_works_when_fetching_a_different_seat() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();

    env.write_seat("a", "acc-a");
    env.write_seat("b", "acc-b");
    env.save_config(&cfg_with_seats(&[("a", "acc-a"), ("b", "acc-b")]));
    let mut state = SeatState::default();
    state.entry_mut("a").cooldown_until = Some(chrono::Utc::now() + chrono::Duration::hours(1));
    env.save_state(&state);

    let client = FakeClient {
        by_seat: Box::new(|_| Ok(snapshot(1, 1))),
        rewrite_slot_tag: None,
    };
    codex_clean::seat_cmd::status_with(&client, Some("b"), false, Some("a")).unwrap();
    assert!(env.load_state().get("a").cooldown_until.is_none());
}


#[test]
fn credits_mention_in_earlier_message_does_not_classify_unrelated_failure() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    env.save_config(&cfg_with_seats(&[("a", "acc-a")]));
    env.save_state(&SeatState::default());

    let attempt = |_args: &[String], _prompt: &str, _mode: &Mode, _scrub: bool| -> anyhow::Result<AttemptResult> {
        let mut output = CodexOutput::default();
        output.messages.push("If your workspace is out of credits, rotation cannot help.".to_string());
        output.messages.push("Now applying the patch…".to_string());
        output.errors.push("invalid_request_error: something unrelated".to_string());
        Ok(AttemptResult {
            output,
            stderr_buffer: Vec::new(),
            stderr_truncated: false,
            stderr_error: None,
            exit_code: 1,
            status_success: false,
            child_exit: 1,
        })
    };
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 1);
    assert!(env.load_state().get("a").cooldown_until.is_none(), "earlier prose must not cool the seat");

    // A long final message that merely mentions credits deep inside is also ignored.
    let attempt = |_args: &[String], _prompt: &str, _mode: &Mode, _scrub: bool| -> anyhow::Result<AttemptResult> {
        let mut output = CodexOutput::default();
        output.messages.push(format!(
            "{} Your workspace is out of credits was the message seen earlier.",
            "Long analysis. ".repeat(30)
        ));
        Ok(AttemptResult {
            output,
            stderr_buffer: Vec::new(),
            stderr_truncated: false,
            stderr_error: None,
            exit_code: 1,
            status_success: false,
            child_exit: 1,
        })
    };
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 1);
    assert!(env.load_state().get("a").cooldown_until.is_none());
}

#[test]
fn incident_jsonl_stream_out_of_credits_as_final_message_is_detected() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    env.save_config(&cfg_with_seats(&[("a", "acc-a")]));
    env.save_state(&SeatState::default());

    // Shape of the 2026-09-06 incident as `codex exec --json` presents it: the
    // provider notice arrives as the final agent message; exit code 1.
    let stream = concat!(
        "{\"type\":\"thread.started\",\"thread_id\":\"01a07804\"}\n",
        "{\"type\":\"turn.started\"}\n",
        "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"Your workspace is out of credits. Add credits to continue.\"}}\n",
        "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":0,\"cached_input_tokens\":0,\"output_tokens\":0}}\n"
    );
    let attempt = move |_args: &[String], _prompt: &str, _mode: &Mode, _scrub: bool| -> anyhow::Result<AttemptResult> {
        let output = runner::parse_codex_stream(std::io::BufReader::new(stream.as_bytes()))?;
        Ok(AttemptResult {
            output,
            stderr_buffer: b"Reading additional input from stdin...\n".to_vec(),
            stderr_truncated: false,
            stderr_error: None,
            exit_code: 1,
            status_success: false,
            child_exit: 1,
        })
    };
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 75, "single seat, now cooling");
    let a = env.load_state().get("a");
    assert_eq!(a.cooldown_reason.as_deref(), Some("credits"));
}

#[test]
fn workspace_cooldown_never_shortens_a_siblings_longer_cooldown() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    let mut cfg = cfg_with_seats(&[("main", "ws-1"), ("backup1", "ws-1")]);
    cfg.seats[0].user_id = Some("user-alice".into());
    cfg.seats[1].user_id = Some("user-bob".into());
    env.save_config(&cfg);
    let seat_dir = env.clean_home_path.join("seats");
    fs::create_dir_all(seat_dir.join("main")).unwrap();
    fs::create_dir_all(seat_dir.join("backup1")).unwrap();
    fs::write(seat_dir.join("main/auth.json"), seat::fake_auth_json_for_tests("ws-1", "user-alice", "a")).unwrap();
    fs::write(seat_dir.join("backup1/auth.json"), seat::fake_auth_json_for_tests("ws-1", "user-bob", "b")).unwrap();
    let mut state = SeatState::default();
    let long = chrono::Utc::now() + chrono::Duration::hours(1) + chrono::Duration::minutes(59);
    state.entry_mut("backup1").cooldown_until = Some(long);
    state.entry_mut("backup1").cooldown_reason = Some("rate_limit".into());
    env.save_state(&state);

    let attempt = |_args: &[String], _prompt: &str, _mode: &Mode, _scrub: bool| -> anyhow::Result<AttemptResult> {
        let mut a = rate_limit_attempt();
        a.output.errors = vec!["Your workspace is out of credits. Add credits to continue.".to_string()];
        Ok(a)
    };
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 75);
    let st = env.load_state();
    assert_eq!(st.get("backup1").cooldown_until, Some(long), "longer sibling cooldown kept");
    assert_eq!(st.get("backup1").cooldown_reason.as_deref(), Some("rate_limit"));
    assert_eq!(st.get("main").cooldown_reason.as_deref(), Some("credits"));
}

#[test]
fn status_propagates_workspace_wide_exhaustion_to_siblings() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    let mut cfg = cfg_with_seats(&[("main", "ws-1"), ("backup1", "ws-1"), ("other", "ws-2")]);
    cfg.seats[0].user_id = Some("user-alice".into());
    cfg.seats[1].user_id = Some("user-bob".into());
    env.save_config(&cfg);
    for (n, u) in [("main", "user-alice"), ("backup1", "user-bob"), ("other", "user-other")] {
        let d = env.clean_home_path.join("seats").join(n);
        fs::create_dir_all(&d).unwrap();
        let aid = if n == "other" { "ws-2" } else { "ws-1" };
        fs::write(d.join("auth.json"), seat::fake_auth_json_for_tests(aid, u, n)).unwrap();
    }
    env.save_state(&SeatState::default());

    let client = FakeClient {
        by_seat: Box::new(|_| {
            let mut snap = snapshot(10, 10);
            snap.buckets[0].rate_limit_reached_type = Some("workspace_owner_credits_depleted".into());
            Ok(snap)
        }),
        rewrite_slot_tag: None,
    };
    // Only `main` is queried, yet backup1 shares the workspace.
    codex_clean::seat_cmd::status_with(&client, Some("main"), false, None).unwrap();
    let st = env.load_state();
    assert_eq!(st.get("main").cooldown_reason.as_deref(), Some("credits"));
    assert_eq!(st.get("backup1").cooldown_reason.as_deref(), Some("credits"), "sibling cooled");
    assert!(st.get("other").cooldown_until.is_none(), "different workspace untouched");
}

#[cfg(unix)]
#[test]
fn private_log_refuses_loose_file_it_cannot_tighten_and_rotates_when_large() {
    use std::os::unix::fs::PermissionsExt;
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    let path = env.clean_home_path.join("seat-events.log");

    // Normal: created 0600.
    seat::append_private_log(&path, "one\n").unwrap();
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);

    // Loosened by someone else: tightened on next write (we own it, so it succeeds).
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    seat::append_private_log(&path, "two\n").unwrap();
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);

    // Symlink in place of the log: refused.
    let decoy = env.clean_home_path.join("decoy.log");
    fs::write(&decoy, "").unwrap();
    let link = env.clean_home_path.join("linked.log");
    std::os::unix::fs::symlink(&decoy, &link).unwrap();
    assert!(seat::append_private_log(&link, "x\n").is_err());
    assert_eq!(fs::read_to_string(&decoy).unwrap(), "");

    // Rotation: a file at the cap is moved aside before the next append,
    // replacing any earlier rotated file.
    let rotated = env.clean_home_path.join("seat-events.log.1");
    fs::write(&rotated, "stale").unwrap();
    let big = vec![b'z'; seat::PRIVATE_LOG_ROTATE_BYTES as usize];
    fs::write(&path, &big).unwrap();
    seat::append_private_log(&path, "after\n").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "after\n");
    assert_eq!(fs::metadata(&rotated).unwrap().len(), big.len() as u64);

    // If rotation cannot happen, the write is refused rather than growing the log.
    fs::write(&path, &big).unwrap();
    fs::remove_file(&rotated).unwrap();
    fs::create_dir(&rotated).unwrap(); // a directory in the way: remove_file fails
    assert!(seat::append_private_log(&path, "nope\n").is_err());
    assert_eq!(fs::metadata(&path).unwrap().len(), big.len() as u64, "log not appended to");
    fs::remove_dir(&rotated).unwrap();
}

// ===========================================================================
// Strategies: fixed and balanced
// ===========================================================================

#[test]
fn fixed_strategy_uses_preferred_seat_and_overflows_when_it_is_cooling() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("main", "acc-main");
    env.write_seat("backup1", "acc-backup1");
    let mut cfg = cfg_with_seats(&[("main", "acc-main"), ("backup1", "acc-backup1")]);
    cfg.rotation.strategy = Strategy::Fixed;
    cfg.rotation.fixed_seat = Some("main".into());
    env.save_config(&cfg);
    // main used a second ago: LRU would pick backup1; fixed picks main.
    let mut state = SeatState::default();
    state.entry_mut("main").last_used = Some(chrono::Utc::now());
    env.save_state(&state);

    let codex_home = env.codex_home_path.clone();
    let attempt = mock_attempt(&codex_home, |aid| {
        assert_eq!(aid, "acc-main");
        ok_attempt()
    });
    assert_eq!(runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap(), 0);

    // main rate-limits → overflow to backup1 within the same run.
    env.save_state(&SeatState::default());
    let attempt = mock_attempt(&codex_home, |aid| match aid {
        "acc-main" => rate_limit_attempt(),
        _ => ok_attempt(),
    });
    assert_eq!(runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap(), 0);
    assert_eq!(env.load_state().active_seat.as_deref(), Some("backup1"));
}

#[test]
fn balanced_strategy_refreshes_stale_snapshots_and_picks_most_headroom() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("main", "acc-main");
    env.write_seat("backup1", "acc-backup1");
    let mut cfg = cfg_with_seats(&[("main", "acc-main"), ("backup1", "acc-backup1")]);
    cfg.rotation.strategy = Strategy::Balanced;
    cfg.rotation.balance_refresh_seconds = 600;
    env.save_config(&cfg);

    // main has a fresh snapshot (weekly 86%); backup1 has none → stale.
    let mut state = SeatState::default();
    state.entry_mut("main").usage = Some(snapshot(20, 86));
    state.entry_mut("main").last_used = Some(chrono::Utc::now() - chrono::Duration::hours(3));
    state.entry_mut("backup1").last_used = Some(chrono::Utc::now());
    env.save_state(&state);

    let fetched = std::sync::Mutex::new(Vec::<String>::new());
    struct CountingClient<'a> {
        fetched: &'a std::sync::Mutex<Vec<String>>,
    }
    impl UsageClient for CountingClient<'_> {
        fn fetch(&self, s: &SE) -> Result<UsageSnapshot, UsageFetchError> {
            self.fetched.lock().unwrap().push(s.name.clone());
            Ok(match s.name.as_str() {
                "backup1" => snapshot(60, 4), // tightest window 60 < main's 86
                _ => snapshot(20, 86),
            })
        }
    }
    let client = CountingClient { fetched: &fetched };

    let codex_home = env.codex_home_path.clone();
    let attempt = mock_attempt(&codex_home, |aid| {
        assert_eq!(aid, "acc-backup1", "seat with the most headroom must be picked");
        ok_attempt()
    });
    let exit = runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &client).unwrap();
    assert_eq!(exit, 0);
    assert_eq!(*fetched.lock().unwrap(), vec!["backup1".to_string()], "only the stale seat is refreshed");
    let st = env.load_state();
    assert!(st.get("backup1").usage.is_some(), "refreshed snapshot recorded");

    // Second run: both snapshots fresh → no fetch at all; still backup1 (60 < 86).
    fetched.lock().unwrap().clear();
    let attempt = mock_attempt(&codex_home, |aid| {
        assert_eq!(aid, "acc-backup1");
        ok_attempt()
    });
    runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &client).unwrap();
    assert!(fetched.lock().unwrap().is_empty(), "fresh snapshots are not refetched");

    // Once backup1 has caught up past main, main is picked.
    let mut st = env.load_state();
    st.entry_mut("backup1").usage = Some(snapshot(90, 40));
    env.save_state(&st);
    let attempt = mock_attempt(&codex_home, |aid| {
        assert_eq!(aid, "acc-main");
        ok_attempt()
    });
    runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &client).unwrap();

    // A refresh that shows a seat exhausted cools it before it is picked.
    struct ExhaustedClient;
    impl UsageClient for ExhaustedClient {
        fn fetch(&self, s: &SE) -> Result<UsageSnapshot, UsageFetchError> {
            Ok(if s.name == "main" { snapshot(100, 50) } else { snapshot(10, 10) })
        }
    }
    let mut st = env.load_state();
    st.entry_mut("main").usage = None;
    st.entry_mut("backup1").usage = None;
    env.save_state(&st);
    let attempt = mock_attempt(&codex_home, |aid| {
        assert_eq!(aid, "acc-backup1", "main is exhausted per the fresh snapshot");
        ok_attempt()
    });
    runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &ExhaustedClient).unwrap();
    assert!(env.load_state().get("main").cooldown_until.is_some());

    // A failing refresh never blocks the run.
    struct FailingClient;
    impl UsageClient for FailingClient {
        fn fetch(&self, _: &SE) -> Result<UsageSnapshot, UsageFetchError> {
            Err(UsageFetchError::CodexMissing)
        }
    }
    let mut st = env.load_state();
    st.entry_mut("backup1").usage = None;
    env.save_state(&st);
    let attempt = mock_attempt(&codex_home, |_| ok_attempt());
    assert_eq!(runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &FailingClient).unwrap(), 0);
}

#[test]
fn balanced_refresh_syncs_rotated_active_token_even_when_run_is_blocked() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    let mut cfg = cfg_with_seats(&[("a", "acc-a")]);
    cfg.rotation.strategy = Strategy::Balanced;
    env.save_config(&cfg);
    let state = SeatState { active_seat: Some("a".to_string()), ..Default::default() };
    env.save_state(&state);
    fs::write(env.codex_home_path.join("auth.json"), fake_auth_json("acc-a")).unwrap();

    // The app-server rotates a's token in its slot and reports it exhausted.
    let client = FakeClient {
        by_seat: Box::new(|_| Ok(snapshot(100, 50))),
        rewrite_slot_tag: Some("rotated-during-refresh".into()),
    };
    let attempt = |_args: &[String], _prompt: &str, _mode: &Mode, _scrub: bool| -> anyhow::Result<AttemptResult> {
        panic!("seat is exhausted; must not run")
    };
    let exit = runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &client).unwrap();
    assert_eq!(exit, 75);
    let global = fs::read_to_string(env.codex_home_path.join("auth.json")).unwrap();
    assert!(global.contains("fake-access-rotated-during-refresh"), "global auth must follow the rotated slot");

    // And the following run must not roll the slot back.
    let mut st = env.load_state();
    st.entry_mut("a").cooldown_until = None;
    st.entry_mut("a").usage = Some(snapshot(1, 1));
    env.save_state(&st);
    let attempt = |_args: &[String], _prompt: &str, _mode: &Mode, _scrub: bool| Ok(ok_attempt());
    runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &client).unwrap();
    assert!(slot_contents(&env, "a").contains("fake-access-rotated-during-refresh"));
}

#[test]
fn balanced_refresh_auth_failure_marks_needs_login_and_picks_other_seat() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("dead", "acc-dead");
    env.write_seat("live", "acc-live");
    let mut cfg = cfg_with_seats(&[("dead", "acc-dead"), ("live", "acc-live")]);
    cfg.rotation.strategy = Strategy::Balanced;
    env.save_config(&cfg);
    let mut state = SeatState::default();
    state.entry_mut("live").usage = Some(snapshot(70, 70)); // known, fairly used
    env.save_state(&state);

    let client = FakeClient {
        by_seat: Box::new(|s| {
            if s.name == "dead" { Err(UsageFetchError::AuthRequired) } else { Ok(snapshot(70, 70)) }
        }),
        rewrite_slot_tag: None,
    };
    let codex_home = env.codex_home_path.clone();
    let attempt = mock_attempt(&codex_home, |aid| {
        assert_eq!(aid, "acc-live", "a seat whose tokens were rejected must not be picked");
        ok_attempt()
    });
    assert_eq!(runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &client).unwrap(), 0);
    let st = env.load_state();
    assert!(st.get("dead").needs_login);
    let log = fs::read_to_string(env.clean_home_path.join("seat-events.log")).unwrap();
    assert!(log.contains("auth_error seat=dead"), "{}", log);
}

#[test]
fn zero_cooldown_fixed_and_balanced_still_try_the_other_seat() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for strategy in [Strategy::Fixed, Strategy::Balanced] {
        let env = TestEnv::new();
        env.write_seat("a", "acc-a");
        env.write_seat("b", "acc-b");
        let mut cfg = cfg_with_seats_min_cooldown(&[("a", "acc-a"), ("b", "acc-b")], 0);
        cfg.rotation.default_cooldown_seconds = 0;
        cfg.rotation.cooldown_max_seconds = 0;
        cfg.rotation.strategy = strategy;
        cfg.rotation.fixed_seat = Some("a".into());
        env.save_config(&cfg);
        let mut state = SeatState::default();
        state.entry_mut("a").usage = Some(snapshot(1, 1));
        state.entry_mut("b").usage = Some(snapshot(50, 50));
        env.save_state(&state);

        let codex_home = env.codex_home_path.clone();
        let attempt = mock_attempt(&codex_home, |aid| match aid {
            "acc-a" => {
                let mut r = rate_limit_attempt();
                r.output.errors = vec!["You've hit your usage limit. Try again later.".to_string()];
                r
            }
            _ => ok_attempt(),
        });
        let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
        assert_eq!(exit, 0, "{:?}: after a fails with an instantly-expired cooldown, b must be tried", strategy);
        assert_eq!(env.load_state().active_seat.as_deref(), Some("b"));
    }
}

#[test]
fn removing_the_fixed_seat_resets_the_strategy_and_config_stays_loadable() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    env.write_seat("b", "acc-b");
    let mut cfg = cfg_with_seats(&[("a", "acc-a"), ("b", "acc-b")]);
    cfg.rotation.strategy = Strategy::Fixed;
    cfg.rotation.fixed_seat = Some("a".into());
    env.save_config(&cfg);

    codex_clean::seat_cmd::remove("a", true).unwrap();
    let cfg = SeatConfig::load().unwrap().unwrap();
    assert_eq!(cfg.rotation.strategy, Strategy::LeastRecentlyUsed);
    assert!(cfg.rotation.fixed_seat.is_none());
    assert_eq!(cfg.seats.len(), 1);

    // Saving an invalid config is refused rather than written.
    let mut bad = cfg.clone();
    bad.rotation.strategy = Strategy::Fixed;
    bad.rotation.fixed_seat = Some("gone".into());
    assert!(bad.save().is_err());
    assert!(SeatConfig::load().is_ok());
}


// ---------------------------------------------------------------------------
// Process-level stdout contract (unix only: fake `codex exec` on PATH)
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn install_fake_codex_exec(dir: &Path, jsonl: &str, exit_code: i32) {
    use std::os::unix::fs::PermissionsExt;
    let script = format!(
        "#!/bin/bash\nif [ \"$1\" != exec ]; then echo \"fake codex: unexpected args: $*\" >&2; exit 2; fi\ncat <<'JSONL'\n{jsonl}\nJSONL\nexit {exit_code}\n"
    );
    let path = dir.join("codex");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
fn run_binary(env: &TestEnv, bin_dir: &Path, extra_env: &[(&str, &str)]) -> (i32, String) {
    let mut path = bin_dir.as_os_str().to_os_string();
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_codex-clean"));
    cmd.arg("say hi")
        .env("PATH", path)
        .env("CODEX_CLEAN_HOME", &env.clean_home_path)
        .env("CODEX_HOME", &env.codex_home_path)
        .env_remove("CODEX_CLEAN_SEAT")
        .env_remove("CODEX_CLEAN_NO_SEAT_LINE");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(unix)]
#[test]
fn stdout_contract_seat_line_position_and_opt_out() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    env.save_config(&cfg_with_seats(&[("a", "acc-a")]));
    let mut state = SeatState::default();
    state.entry_mut("a").usage = Some(snapshot(48, 14));
    env.save_state(&state);
    let bin = tempfile::tempdir().unwrap();

    // Full run: Session → message → Tokens → Seat, and nothing after.
    install_fake_codex_exec(
        bin.path(),
        concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"t-1\"}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"Hi!\"}}\n",
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":10,\"cached_input_tokens\":2,\"output_tokens\":3}}"
        ),
        0,
    );
    let (code, out) = run_binary(&env, bin.path(), &[]);
    assert_eq!(code, 0, "{}", out);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "Session: t-1");
    let tokens = lines.iter().position(|l| l.starts_with("Tokens: ")).expect("Tokens line");
    let seat = lines.iter().position(|l| l.starts_with("Seat: ")).expect("Seat line");
    assert_eq!(seat, tokens + 1, "Seat follows Tokens: {:?}", lines);
    assert_eq!(seat, lines.len() - 1, "Seat is last (healthy pool → no Seats trailer): {:?}", lines);
    assert_eq!(out.matches("Seat: ").count(), 1, "exactly once");
    assert!(lines[seat].starts_with("Seat: a (least-recently-used; usage 5h 48% wk 14%, as of "), "{}", lines[seat]);

    // Failure before usage is reported: no Tokens line; Seat still present, last, once.
    install_fake_codex_exec(
        bin.path(),
        concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"t-2\"}\n",
            "{\"type\":\"turn.failed\",\"error\":{\"message\":\"invalid_request_error: nope\"}}"
        ),
        1,
    );
    let (code, out) = run_binary(&env, bin.path(), &[]);
    assert_eq!(code, 1);
    assert!(!out.contains("Tokens: "), "{}", out);
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines.last().unwrap().starts_with("Seat: a ("), "{:?}", lines);
    assert!(lines.last().unwrap().contains("run failed"), "{:?}", lines);
    assert_eq!(out.matches("Seat: ").count(), 1);

    // Opt-out suppresses the line.
    let (_, out) = run_binary(&env, bin.path(), &[("CODEX_CLEAN_NO_SEAT_LINE", "1")]);
    assert!(!out.contains("Seat: "), "{}", out);

    // Degraded pool: Seat line, blank line, then the Seats trailer.
    let mut st = env.load_state();
    st.entry_mut("a").needs_login = false;
    env.save_state(&st);
    env.write_seat("b", "acc-b");
    env.save_config(&cfg_with_seats(&[("a", "acc-a"), ("b", "acc-b")]));
    let mut st = env.load_state();
    st.entry_mut("b").needs_login = true;
    env.save_state(&st);
    let (_, out) = run_binary(&env, bin.path(), &[]);
    let lines: Vec<&str> = out.lines().collect();
    let seat = lines.iter().position(|l| l.starts_with("Seat: ")).unwrap();
    assert_eq!(lines[seat + 1], "", "{:?}", lines);
    assert!(lines[seat + 2].starts_with("Seats: b needs login"), "{:?}", lines);

    // No seats configured: passthrough prints neither line.
    fs::remove_file(env.clean_home_path.join("seats.toml")).unwrap();
    let (_, out) = run_binary(&env, bin.path(), &[]);
    assert!(!out.contains("Seat: ") && !out.contains("Seats: "), "{}", out);
}
