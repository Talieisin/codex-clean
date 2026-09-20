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
        std::env::remove_var("CODEX_CLEAN_USE_CREDITS");
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
        std::env::remove_var("CODEX_CLEAN_USE_CREDITS");
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
        credits: Some(seat::UsageCredits { has_credits: false, unlimited: false, balance: None }),
        spend_control_reached: Some(false),
            resets: None,
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
        matches!(usage::verdict(&snap, chrono::Utc::now()), usage::UsageVerdict::Exhausted { .. }),
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
    assert_eq!(
        st.get("backup1").cooldown_reason.as_deref(),
        Some("credits"),
        "the stronger workspace-wide reason is recorded, not hidden behind rate_limit"
    );
    assert_eq!(st.get("main").cooldown_reason.as_deref(), Some("credits"));
}

#[test]
fn credits_failure_leaves_a_sibling_with_cached_headroom_runnable() {
    // The run path, not the snapshot path: `main` fails with a credits error
    // and `backup1` shares its workspace but has a cached reading showing
    // plenty of included quota. `backup1` runs on that quota for free, so the
    // credits blocker must not reach it. Before the blocker was scoped, it
    // was cooled alongside `main` and the run ended 75 with a usable seat
    // sitting idle.
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
    // backup1 used more recently, so LRU reaches for main first.
    state.entry_mut("backup1").last_used = Some(chrono::Utc::now());
    state.entry_mut("backup1").usage = Some(snapshot(10, 10));
    env.save_state(&state);

    // First attempt (main) fails for credits; the second (backup1) succeeds.
    let calls = RefCell::new(0usize);
    let attempt = move |_args: &[String], _prompt: &str, _mode: &Mode, _scrub: bool| -> anyhow::Result<AttemptResult> {
        *calls.borrow_mut() += 1;
        if *calls.borrow() == 1 {
            let mut a = rate_limit_attempt();
            a.output.errors = vec!["Your workspace is out of credits. Add credits to continue.".to_string()];
            return Ok(a);
        }
        Ok(ok_attempt())
    };
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();

    assert_eq!(exit, 0, "the run falls through to the seat that still has quota");
    let st = env.load_state();
    assert_eq!(st.get("main").cooldown_reason.as_deref(), Some("credits"), "the seat that failed is cooled");
    assert!(
        st.get("backup1").cooldown_until.is_none(),
        "a sibling with included quota left is not cooled by the workspace credits blocker"
    );
    assert!(st.get("backup1").is_eligible(chrono::Utc::now()), "and stays eligible for the next run");
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


// ===========================================================================
// Credit policy: ask / never / always, grants, exit 77, probes
// ===========================================================================

use codex_clean::runner::{CreditChoice, CreditDecider, EXIT_CREDITS_CONSENT_NEEDED};
use codex_clean::seat::{CreditPolicy, UsageCredits};

/// Weekly window at 100% (resets in 3 days) with workspace credits available.
fn on_credits_snapshot() -> UsageSnapshot {
    let mut s = snapshot(0, 100);
    s.credits = Some(UsageCredits { has_credits: true, unlimited: false, balance: Some("25".into()) });
    s
}

/// Two seats in one Team workspace, both with included quota used up and
/// credits available — the 2026-09-11 situation.
fn setup_both_on_credits(env: &TestEnv, policy: CreditPolicy) {
    let mut cfg = cfg_with_seats(&[("main", "ws-1"), ("backup1", "ws-1")]);
    cfg.seats[0].user_id = Some("user-alice".into());
    cfg.seats[1].user_id = Some("user-bob".into());
    cfg.rotation.credits = policy;
    env.save_config(&cfg);
    let dir = env.clean_home_path.join("seats");
    for (n, u) in [("main", "user-alice"), ("backup1", "user-bob")] {
        fs::create_dir_all(dir.join(n)).unwrap();
        fs::write(dir.join(n).join("auth.json"), seat::fake_auth_json_for_tests("ws-1", u, n)).unwrap();
    }
    let mut state = SeatState::default();
    state.entry_mut("main").usage = Some(on_credits_snapshot());
    state.entry_mut("backup1").usage = Some(on_credits_snapshot());
    state.entry_mut("main").last_used = Some(chrono::Utc::now() - chrono::Duration::hours(1));
    env.save_state(&state);
}

struct FixedDecider {
    choice: CreditChoice,
    calls: std::cell::Cell<u32>,
}

impl CreditDecider for FixedDecider {
    fn decide(&self, seats: &[(String, Option<chrono::DateTime<chrono::Utc>>)]) -> CreditChoice {
        self.calls.set(self.calls.get() + 1);
        assert!(!seats.is_empty());
        // The lock must be released while deciding.
        assert!(
            seat::CodexLock::try_acquire().unwrap().is_some(),
            "CodexLock must not be held while waiting for a credits decision"
        );
        self.choice
    }
}

fn decider(choice: CreditChoice) -> FixedDecider {
    FixedDecider { choice, calls: std::cell::Cell::new(0) }
}

fn run_deps(
    attempt: impl Fn(&[String], &str, &Mode, bool) -> anyhow::Result<AttemptResult>,
    d: &dyn CreditDecider,
) -> i32 {
    runner::run_codex_with_deps(&[], "hi", Mode::Exec, attempt, &usage::NoUsageClient, d).unwrap()
}

#[test]
fn background_ask_with_quota_used_and_credits_exits_77_without_running() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_both_on_credits(&env, CreditPolicy::Ask);
    let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        panic!("must not spend credits without consent")
    };
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, EXIT_CREDITS_CONSENT_NEEDED);
    let log = fs::read_to_string(env.clean_home_path.join("seat-events.log")).unwrap();
    assert!(log.contains("quota_exhausted_credits_available"), "{}", log);
    assert!(log.contains("choice=wait"), "{}", log);
}

#[test]
fn env_consent_must_be_exactly_1() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_both_on_credits(&env, CreditPolicy::Ask);
    for v in ["0", "yes", "true", ""] {
        std::env::set_var("CODEX_CLEAN_USE_CREDITS", v);
        let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
            panic!("CODEX_CLEAN_USE_CREDITS={:?} must not count as consent", v)
        };
        assert_eq!(runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap(), EXIT_CREDITS_CONSENT_NEEDED);
    }
    std::env::set_var("CODEX_CLEAN_USE_CREDITS", "1");
    let codex_home = env.codex_home_path.clone();
    let attempt = mock_attempt(&codex_home, |_| ok_attempt());
    assert_eq!(runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap(), 0);
    let log = fs::read_to_string(env.clean_home_path.join("seat-events.log")).unwrap();
    assert!(log.contains("on_credits seat=") && log.contains("(this run)"), "{}", log);
    assert!(
        seat::SCRUB_ENV_VARS_FOR_TESTS.contains(&"CODEX_CLEAN_USE_CREDITS"),
        "consent must never reach the codex child"
    );
}

#[test]
fn never_mode_needs_explicit_consent_and_always_mode_spends() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_both_on_credits(&env, CreditPolicy::Never);
    let prompted = decider(CreditChoice::ThisRun);
    let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        panic!("never mode must not prompt or spend")
    };
    assert_eq!(run_deps(attempt, &prompted), EXIT_CREDITS_CONSENT_NEEDED);
    assert_eq!(prompted.calls.get(), 0, "never mode does not prompt");
    std::env::set_var("CODEX_CLEAN_USE_CREDITS", "1");
    let codex_home = env.codex_home_path.clone();
    assert_eq!(run_deps(mock_attempt(&codex_home, |_| ok_attempt()), &prompted), 0);
    std::env::remove_var("CODEX_CLEAN_USE_CREDITS");

    let env2 = TestEnv::new();
    setup_both_on_credits(&env2, CreditPolicy::Always);
    let codex_home = env2.codex_home_path.clone();
    let attempt = mock_attempt(&codex_home, |_| ok_attempt());
    assert_eq!(runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap(), 0);
}

#[test]
fn prompt_answers_this_run_until_reset_always_and_wait() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // this run
    let env = TestEnv::new();
    setup_both_on_credits(&env, CreditPolicy::Ask);
    let d = decider(CreditChoice::ThisRun);
    let codex_home = env.codex_home_path.clone();
    assert_eq!(run_deps(mock_attempt(&codex_home, |_| ok_attempt()), &d), 0);
    assert_eq!(d.calls.get(), 1);
    assert!(env.load_state().credit_grants.is_empty(), "this-run consent is not persisted");

    // until quota resets → a workspace grant that later runs reuse without asking
    let env = TestEnv::new();
    setup_both_on_credits(&env, CreditPolicy::Ask);
    let d = decider(CreditChoice::UntilReset);
    let codex_home = env.codex_home_path.clone();
    assert_eq!(run_deps(mock_attempt(&codex_home, |_| ok_attempt()), &d), 0);
    let st = env.load_state();
    assert_eq!(st.credit_grants.len(), 1);
    let until = *st.credit_grants.values().next().unwrap();
    assert!(until > chrono::Utc::now() + chrono::Duration::days(2), "until the quota reset");
    let d2 = decider(CreditChoice::Wait);
    assert_eq!(run_deps(mock_attempt(&codex_home, |_| ok_attempt()), &d2), 0);
    assert_eq!(d2.calls.get(), 0, "an active grant needs no prompt");
    // After the grant expires the prompt/77 comes back.
    let mut st = env.load_state();
    st.credit_grants.insert("acct:ws-1".into(), chrono::Utc::now() - chrono::Duration::seconds(1));
    fs::write(env.clean_home_path.join("state.json"), serde_json::to_string(&st).unwrap()).unwrap();
    let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        panic!("expired grant must not authorise")
    };
    assert_eq!(runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap(), EXIT_CREDITS_CONSENT_NEEDED);

    // always → persisted mode
    let env = TestEnv::new();
    setup_both_on_credits(&env, CreditPolicy::Ask);
    let d = decider(CreditChoice::Always);
    let codex_home = env.codex_home_path.clone();
    assert_eq!(run_deps(mock_attempt(&codex_home, |_| ok_attempt()), &d), 0);
    assert_eq!(SeatConfig::load().unwrap().unwrap().rotation.credits, CreditPolicy::Always);

    // wait → 77, nothing spent
    let env = TestEnv::new();
    setup_both_on_credits(&env, CreditPolicy::Ask);
    let d = decider(CreditChoice::Wait);
    let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        panic!("wait must not spend")
    };
    assert_eq!(run_deps(attempt, &d), EXIT_CREDITS_CONSENT_NEEDED);
    drop(env);
}

#[test]
fn in_quota_seat_is_used_before_credits_even_with_consent() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for strategy in [Strategy::LeastRecentlyUsed, Strategy::Balanced, Strategy::Fixed] {
        let env = TestEnv::new();
        setup_both_on_credits(&env, CreditPolicy::Always);
        let mut cfg = SeatConfig::load().unwrap().unwrap();
        cfg.rotation.strategy = strategy;
        cfg.rotation.fixed_seat = Some("main".into());
        env.save_config(&cfg);
        let mut st = env.load_state();
        st.entry_mut("backup1").usage = Some(snapshot(10, 40)); // still in quota
        st.entry_mut("backup1").last_used = Some(chrono::Utc::now());
        env.save_state(&st);
        let codex_home = env.codex_home_path.clone();
        let seen = std::rc::Rc::new(RefCell::new(String::new()));
        let seen2 = seen.clone();
        let attempt = move |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
            *seen2.borrow_mut() = fs::read_to_string(codex_home.join("auth.json"))?;
            Ok(ok_attempt())
        };
        assert_eq!(runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap(), 0);
        assert!(seen.borrow().contains("fake-access-backup1"), "{:?}: in-quota seat first", strategy);
    }
}

#[test]
fn decision_after_a_failed_attempt_keeps_retry_budget_and_output() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_both_on_credits(&env, CreditPolicy::Ask);
    // backup1 in quota (tried first), main on credits.
    let mut st = env.load_state();
    st.entry_mut("backup1").usage = Some(snapshot(10, 40));
    env.save_state(&st);
    let codex_home = env.codex_home_path.clone();
    let calls = std::rc::Rc::new(RefCell::new(Vec::<String>::new()));
    let calls2 = calls.clone();
    let attempt = move |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        let auth = fs::read_to_string(codex_home.join("auth.json"))?;
        let who = if auth.contains("fake-access-backup1") { "backup1" } else { "main" };
        calls2.borrow_mut().push(who.to_string());
        Ok(if who == "backup1" { rate_limit_attempt() } else { ok_attempt() })
    };
    // max_retries = 1 → two attempts total; the decision must not use one up.
    let d = decider(CreditChoice::ThisRun);
    assert_eq!(run_deps(attempt, &d), 0);
    assert_eq!(*calls.borrow(), vec!["backup1".to_string(), "main".to_string()]);
    assert_eq!(d.calls.get(), 1);

    // Same with "wait": 77 after the failed attempt, nothing else runs.
    let env = TestEnv::new();
    setup_both_on_credits(&env, CreditPolicy::Ask);
    let mut st = env.load_state();
    st.entry_mut("backup1").usage = Some(snapshot(10, 40));
    env.save_state(&st);
    let codex_home = env.codex_home_path.clone();
    let attempt = mock_attempt(&codex_home, |aid| {
        assert_eq!(aid, "ws-1");
        rate_limit_attempt()
    });
    let d = decider(CreditChoice::Wait);
    assert_eq!(run_deps(attempt, &d), EXIT_CREDITS_CONSENT_NEEDED);
    assert!(env.load_state().get("backup1").cooldown_until.is_some());
}

#[test]
fn decision_reentry_reloads_config_changed_while_unlocked() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_both_on_credits(&env, CreditPolicy::Ask);
    struct EditingDecider;
    impl CreditDecider for EditingDecider {
        fn decide(&self, _: &[(String, Option<chrono::DateTime<chrono::Utc>>)]) -> CreditChoice {
            // Another process switches the policy to always while we wait.
            let _lock = seat::CodexLock::try_acquire().unwrap().expect("lock is free");
            let mut cfg = SeatConfig::load().unwrap().unwrap();
            cfg.rotation.credits = CreditPolicy::Always;
            cfg.save().unwrap();
            CreditChoice::Wait
        }
    }
    let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        panic!("wait chosen; must not run")
    };
    // The user chose wait, so nothing is spent — but consent is no longer the
    // blocker (always was set meanwhile), so the exit is 75, not 77.
    assert_eq!(run_deps(attempt, &EditingDecider), 75);
    // The next invocation sees the edited config and spends.
    let codex_home = env.codex_home_path.clone();
    assert_eq!(runner::run_codex_with(&[], "hi", Mode::Exec, mock_attempt(&codex_home, |_| ok_attempt())).unwrap(), 0);
}

#[test]
fn pinned_on_credits_seat_needs_consent_and_never_rotates() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_both_on_credits(&env, CreditPolicy::Ask);
    let mut st = env.load_state();
    st.entry_mut("backup1").usage = Some(snapshot(10, 40)); // in quota, but not pinned
    env.save_state(&st);
    std::env::set_var("CODEX_CLEAN_SEAT", "main");
    let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        panic!("pinned on-credits seat must not run without consent, nor rotate")
    };
    assert_eq!(runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap(), EXIT_CREDITS_CONSENT_NEEDED);
    std::env::set_var("CODEX_CLEAN_USE_CREDITS", "1");
    let codex_home = env.codex_home_path.clone();
    let seen = std::rc::Rc::new(RefCell::new(String::new()));
    let seen2 = seen.clone();
    let attempt = move |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        *seen2.borrow_mut() = fs::read_to_string(codex_home.join("auth.json"))?;
        Ok(ok_attempt())
    };
    assert_eq!(runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap(), 0);
    assert!(seen.borrow().contains("fake-access-main"), "the pinned seat ran");
}

/// Client that returns a fixed snapshot per call and counts calls.
struct CountingClient {
    snap: UsageSnapshot,
    calls: std::sync::atomic::AtomicUsize,
    fail: bool,
}

impl UsageClient for CountingClient {
    fn fetch(&self, _: &SE) -> Result<UsageSnapshot, UsageFetchError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail {
            Err(UsageFetchError::Timeout(std::time::Duration::from_secs(1)))
        } else {
            let mut s = self.snap.clone();
            s.fetched_at = chrono::Utc::now();
            Ok(s)
        }
    }
}

fn counting(snap: UsageSnapshot, fail: bool) -> CountingClient {
    CountingClient { snap, calls: std::sync::atomic::AtomicUsize::new(0), fail }
}

#[test]
fn credits_purchase_unblocks_seats_cooling_from_before_without_a_status_call() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for (strategy, policy, expected) in [
        (Strategy::LeastRecentlyUsed, CreditPolicy::Always, 0),
        (Strategy::Balanced, CreditPolicy::Always, 0),
        (Strategy::LeastRecentlyUsed, CreditPolicy::Ask, EXIT_CREDITS_CONSENT_NEEDED),
    ] {
        let env = TestEnv::new();
        setup_both_on_credits(&env, policy);
        let mut cfg = SeatConfig::load().unwrap().unwrap();
        cfg.rotation.strategy = strategy;
        env.save_config(&cfg);
        // The pre-fix state: both cooling for rate_limit with stale, no-credits snapshots.
        let mut st = env.load_state();
        for n in ["main", "backup1"] {
            let e = st.entry_mut(n);
            e.cooldown_until = Some(chrono::Utc::now() + chrono::Duration::hours(20));
            e.cooldown_reason = Some("rate_limit".into());
            let mut old = snapshot(0, 100);
            old.fetched_at = chrono::Utc::now() - chrono::Duration::hours(2);
            e.usage = Some(old);
        }
        env.save_state(&st);
        let client = counting(on_credits_snapshot(), false);
        let codex_home = env.codex_home_path.clone();
        let exit = runner::run_codex_with_client(&[], "hi", Mode::Exec, mock_attempt(&codex_home, |_| ok_attempt()), &client).unwrap();
        assert_eq!(exit, expected, "{:?}/{:?}", strategy, policy);
        assert!(client.calls.load(std::sync::atomic::Ordering::SeqCst) >= 1, "cooling seats were probed");
        let st = env.load_state();
        assert!(st.get("main").cooldown_until.is_none(), "rate_limit cooldown cleared by credits");
    }
}

#[test]
fn blocked_probe_is_rate_limited_and_failures_do_not_block() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    env.save_config(&cfg_with_seats(&[("a", "acc-a")]));
    let mut st = SeatState::default();
    st.entry_mut("a").cooldown_until = Some(chrono::Utc::now() + chrono::Duration::hours(5));
    st.entry_mut("a").cooldown_reason = Some("rate_limit".into());
    env.save_state(&st);
    // Probe returns "still exhausted, no credits".
    let client = counting(snapshot(0, 100), false);
    let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        panic!("still exhausted")
    };
    assert_eq!(runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &client).unwrap(), 75);
    assert_eq!(client.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    // Within blocked_probe_seconds: no second fetch.
    assert_eq!(runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &client).unwrap(), 75);
    assert_eq!(client.calls.load(std::sync::atomic::Ordering::SeqCst), 1, "rate-limited");

    // Pre-run check: in-quota seat at 85% with a stale reading is re-checked;
    // a failing check still lets the run proceed.
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    env.save_config(&cfg_with_seats(&[("a", "acc-a")]));
    let mut st = SeatState::default();
    let mut old = snapshot(85, 60);
    old.fetched_at = chrono::Utc::now() - chrono::Duration::hours(1);
    st.entry_mut("a").usage = Some(old);
    env.save_state(&st);
    let failing = counting(snapshot(0, 0), true);
    let codex_home = env.codex_home_path.clone();
    let exit = runner::run_codex_with_client(&[], "hi", Mode::Exec, mock_attempt(&codex_home, |_| ok_attempt()), &failing).unwrap();
    assert_eq!(exit, 0, "a failed pre-run check never blocks work");
    assert_eq!(failing.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(env.load_state().get("a").usage_checked_at.is_some());
}

#[test]
fn prerun_check_catches_a_seat_that_crossed_into_credits() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_both_on_credits(&env, CreditPolicy::Ask);
    // Last readings say both are at 90% (stale); really they are at 100% on credits.
    let mut st = env.load_state();
    for n in ["main", "backup1"] {
        let mut old = snapshot(0, 90);
        old.fetched_at = chrono::Utc::now() - chrono::Duration::hours(1);
        st.entry_mut(n).usage = Some(old);
    }
    env.save_state(&st);
    let client = counting(on_credits_snapshot(), false);
    let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        panic!("pre-run check must discover the seats are on credits")
    };
    let exit = runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &client).unwrap();
    assert_eq!(exit, EXIT_CREDITS_CONSENT_NEEDED);
}

#[test]
fn credits_running_out_after_a_clear_recools_workspace() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_both_on_credits(&env, CreditPolicy::Always);
    let mut st = env.load_state();
    st.entry_mut("backup1").needs_login = true;
    env.save_state(&st);
    let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        let mut a = rate_limit_attempt();
        a.output.errors = vec!["Your workspace is out of credits. Add credits to continue.".into()];
        Ok(a)
    };
    let exit = runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap();
    assert_eq!(exit, 75);
    let st = env.load_state();
    assert_eq!(st.get("main").cooldown_reason.as_deref(), Some("credits"));
    assert_eq!(st.get("backup1").cooldown_reason.as_deref(), Some("credits"));
    assert!(st.get("backup1").needs_login, "needs_login preserved");
}

#[test]
fn run_failing_with_reused_refresh_token_marks_needs_login() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    env.save_config(&cfg_with_seats(&[("a", "acc-a")]));
    env.save_state(&SeatState::default());
    let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        let mut a = auth_error_attempt();
        a.output.errors = vec![
            "Your access token could not be refreshed because your refresh token was already used. Please log out and sign in again.".into(),
        ];
        Ok(a)
    };
    assert_eq!(runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap(), 1);
    assert!(env.load_state().get("a").needs_login);
}

#[test]
fn status_marks_needs_login_on_auth_failure_and_does_not_slide() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    env.write_seat("b", "acc-b");
    env.save_config(&cfg_with_seats(&[("a", "acc-a"), ("b", "acc-b")]));
    env.save_state(&SeatState::default());
    let client = FakeClient {
        by_seat: Box::new(|s| {
            if s.name == "a" {
                Err(UsageFetchError::AuthRequired)
            } else {
                let mut snap = snapshot(0, 100);
                snap.buckets[0].windows[1].resets_at = Some(chrono::Utc::now() + chrono::Duration::days(5));
                Ok(snap)
            }
        }),
        rewrite_slot_tag: None,
    };
    codex_clean::seat_cmd::status_with(&client, None, false, None).unwrap();
    let st = env.load_state();
    assert!(st.get("a").needs_login);
    let first = st.get("b").cooldown_until.expect("b cooling");
    for _ in 0..3 {
        codex_clean::seat_cmd::status_with(&client, None, false, None).unwrap();
    }
    assert_eq!(env.load_state().get("b").cooldown_until, Some(first), "status must not slide the cooldown");
}

#[cfg(unix)]
#[test]
fn stdout_consent_warning_and_seat_list_credits_marker() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_both_on_credits(&env, CreditPolicy::Ask);
    let bin = tempfile::tempdir().unwrap();
    install_fake_codex_exec(bin.path(), "{\"type\":\"thread.started\",\"thread_id\":\"t\"}", 0);
    let (code, out) = run_binary(&env, bin.path(), &[]);
    assert_eq!(code, 77, "{}", out);
    let last = out.lines().last().unwrap_or_default().to_string();
    assert!(last.starts_with("Seats: included quota used up on main (resets "), "{}", last);
    assert!(last.contains("needs the user's consent"), "{}", last);
    assert!(last.contains("CODEX_CLEAN_USE_CREDITS=1"), "{}", last);
    assert!(!out.contains("Seat: "), "nothing ran, so no Seat line: {}", out);

    // With consent the run happens and the Seat line says so.
    let (code, out) = run_binary(&env, bin.path(), &[("CODEX_CLEAN_USE_CREDITS", "1")]);
    assert_eq!(code, 0, "{}", out);
    assert!(out.contains("on credits (this run)"), "{}", out);

    // seat list shows the marker without truncation at 100%/100%.
    let mut st = env.load_state();
    st.entry_mut("main").usage = Some({
        let mut s = on_credits_snapshot();
        s.buckets[0].windows[0].used_percent = 100;
        s
    });
    env.save_state(&st);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_codex-clean"))
        .args(["seat", "list"])
        .env("CODEX_CLEAN_HOME", &env.clean_home_path)
        .env("CODEX_HOME", &env.codex_home_path)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("5h 100% wk 100% +credits"), "{}", text);
    assert!(text.contains("quota used; credits not in use (ask)"), "{}", text);
}


// ---------------------------------------------------------------------------
// Regressions from the Codex review of the credit policy
// ---------------------------------------------------------------------------

#[test]
fn partial_status_with_credits_never_turns_a_stale_exhausted_sibling_into_in_quota() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_both_on_credits(&env, CreditPolicy::Ask);
    // backup1: cached "100%, no credits" and cooling (checked before the purchase).
    let mut st = env.load_state();
    let e = st.entry_mut("backup1");
    e.usage = Some(snapshot(0, 100));
    e.cooldown_until = Some(chrono::Utc::now() + chrono::Duration::hours(10));
    e.cooldown_reason = Some("rate_limit".into());
    e.usage_checked_at = Some(chrono::Utc::now());
    env.save_state(&st);
    // `seat status main` sees the workspace credits.
    let client = FakeClient { by_seat: Box::new(|_| Ok(on_credits_snapshot())), rewrite_slot_tag: None };
    codex_clean::seat_cmd::status_with(&client, Some("main"), false, None).unwrap();
    let st = env.load_state();
    assert!(st.get("backup1").cooldown_until.is_none(), "credits lift backup1's rate_limit cooldown");
    assert_eq!(
        usage::quota_state(&st.get("backup1"), chrono::Utc::now()).as_str(),
        "on_credits",
        "backup1's cached reading now carries the workspace credits, so it is gated by consent"
    );
    // A background run must not spend on backup1 without consent.
    let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        panic!("backup1 must not run on credits without consent")
    };
    assert_eq!(runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap(), EXIT_CREDITS_CONSENT_NEEDED);
}

#[test]
fn fresh_exhausted_reading_blocks_a_run_after_the_cooldown_clock_expires() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    env.save_config(&cfg_with_seats(&[("a", "acc-a")]));
    let mut st = SeatState::default();
    let e = st.entry_mut("a");
    e.usage = Some(snapshot(0, 100)); // fresh, weekly 100%, reset in 3 days, no credits
    e.cooldown_until = Some(chrono::Utc::now() - chrono::Duration::seconds(1)); // just expired
    e.cooldown_reason = Some("rate_limit".into());
    e.usage_checked_at = Some(chrono::Utc::now());
    env.save_state(&st);
    let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        panic!("the latest reading says the seat is exhausted")
    };
    assert_eq!(runner::run_codex_with(&[], "hi", Mode::Exec, attempt).unwrap(), 75);
    assert!(env.load_state().get("a").cooldown_until.unwrap() > chrono::Utc::now());
}

#[test]
fn until_reset_consent_is_not_attached_to_a_workspace_that_changed_meanwhile() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_both_on_credits(&env, CreditPolicy::Ask);
    struct ReAddingDecider;
    impl CreditDecider for ReAddingDecider {
        fn decide(&self, _: &[(String, Option<chrono::DateTime<chrono::Utc>>)]) -> CreditChoice {
            // While the question is open both seats are re-registered under a
            // different workspace.
            let _lock = seat::CodexLock::try_acquire().unwrap().expect("lock is free");
            let mut cfg = SeatConfig::load().unwrap().unwrap();
            for s in &mut cfg.seats {
                s.account_id = Some("ws-OTHER".into());
            }
            cfg.save().unwrap();
            CreditChoice::UntilReset
        }
    }
    let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        panic!("consent was given for a different workspace")
    };
    assert_eq!(run_deps(attempt, &ReAddingDecider), EXIT_CREDITS_CONSENT_NEEDED);
    assert!(env.load_state().credit_grants.is_empty(), "no grant for the new workspace");
}

#[test]
fn concurrent_orphan_parking_keeps_every_distinct_blob_once() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    let expected = seat::SeatIdentity { account_id: Some("acc-a".into()), user_id: Some("user-acc-a".into()) };
    let srcs: Vec<PathBuf> = (0..8)
        .map(|i| {
            let p = env.clean_home_path.join(format!("src-{}.json", i));
            // Four distinct foreign blobs, each written twice.
            let who = format!("user-foreign-{}", i % 4);
            fs::write(&p, seat::fake_auth_json_for_tests("acc-x", &who, &who)).unwrap();
            p
        })
        .collect();
    std::thread::scope(|sc| {
        for p in &srcs {
            let expected = expected.clone();
            sc.spawn(move || {
                let out = seat::refresh_back_from_guarded(p, "a", &expected).unwrap();
                assert!(matches!(out, seat::RefreshBackOutcome::SkippedMismatch { orphaned: Some(_), .. }), "{:?}", out);
            });
        }
    });
    let parked: Vec<_> = fs::read_dir(env.clean_home_path.join("orphaned")).unwrap().flatten().collect();
    assert_eq!(parked.len(), 4, "one file per distinct blob, none lost or duplicated");
    for f in parked {
        assert!(f.file_name().to_string_lossy().starts_with("auth-"));
        let body = fs::read_to_string(f.path()).unwrap();
        assert!(body.contains("fake-access-user-foreign-"), "intact content");
    }
}

#[cfg(unix)]
#[test]
fn consent_env_never_reaches_the_codex_child() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    use std::os::unix::fs::PermissionsExt;
    let bin = tempfile::tempdir().unwrap();
    let script = "#!/bin/bash\n\
        echo '{\"type\":\"thread.started\",\"thread_id\":\"t\"}'\n\
        echo \"{\\\"type\\\":\\\"item.completed\\\",\\\"item\\\":{\\\"type\\\":\\\"agent_message\\\",\\\"text\\\":\\\"USE=${CODEX_CLEAN_USE_CREDITS:-unset} NI=${CODEX_CLEAN_NONINTERACTIVE:-unset}\\\"}}\"\n";
    let path = bin.path().join("codex");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();

    // No seats configured: the passthrough path.
    let env = TestEnv::new();
    let extra = [("CODEX_CLEAN_USE_CREDITS", "1"), ("CODEX_CLEAN_NONINTERACTIVE", "1")];
    let (code, out) = run_binary(&env, bin.path(), &extra);
    assert_eq!(code, 0, "{}", out);
    assert!(out.contains("USE=unset NI=unset"), "passthrough leaked consent: {}", out);

    // Multi-seat path.
    env.write_seat("a", "acc-a");
    env.save_config(&cfg_with_seats(&[("a", "acc-a")]));
    let (code, out) = run_binary(&env, bin.path(), &extra);
    assert_eq!(code, 0, "{}", out);
    assert!(out.contains("USE=unset NI=unset"), "multi-seat leaked consent: {}", out);
}

#[cfg(unix)]
#[test]
fn status_json_reports_credits_mode_grants_and_quota_state() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    env.save_config(&cfg_with_seats(&[("a", "acc-a")]));
    let mut st = SeatState::default();
    st.credit_grants.insert("acct:acc-a".into(), chrono::Utc::now() + chrono::Duration::hours(5));
    st.credit_grants.insert("acct:acc-expired".into(), chrono::Utc::now() - chrono::Duration::hours(1));
    env.save_state(&st);
    let bin = tempfile::tempdir().unwrap();
    let canned = r#"{"id":2,"result":{"rateLimits":{"limitId":"codex","planType":"team","primary":{"usedPercent":3,"windowDurationMins":300,"resetsAt":4102444800},"secondary":{"usedPercent":100,"windowDurationMins":10080,"resetsAt":4102448400},"credits":{"hasCredits":true,"unlimited":false,"balance":"12.50"},"rateLimitReachedType":null,"spendControlReached":false}}}"#;
    install_fake_codex(bin.path(), "", &format!("echo '{}'", canned));
    let mut path = bin.path().as_os_str().to_os_string();
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_codex-clean"))
        .args(["seat", "status", "--json"])
        .env("PATH", path)
        .env("CODEX_CLEAN_HOME", &env.clean_home_path)
        .env("CODEX_HOME", &env.codex_home_path)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {}", e, text));
    assert_eq!(v["credits_mode"], "ask");
    let grants = v["credit_grants"].as_array().unwrap();
    assert_eq!(grants.len(), 1, "expired grants omitted: {}", text);
    assert_eq!(grants[0]["seats"], serde_json::json!(["a"]));
    assert!(grants[0]["workspace"].as_str().unwrap().starts_with("ws-"));
    assert!(!text.contains("acc-a"), "account ids stay out of --json: {}", text);
    let seat0 = &v["seats"][0];
    assert_eq!(seat0["quota_state"]["state"], "on_credits");
    assert!(seat0["quota_state"]["resets_at"].is_string());
    assert_eq!(seat0["usage"]["credits"]["balance"], "12.50");
}


#[test]
fn this_run_consent_does_not_cover_a_workspace_changed_while_unlocked() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_both_on_credits(&env, CreditPolicy::Ask);
    /// Says "this run" to the first question (about ws-1) while re-registering
    /// the seats under ws-OTHER, then "wait" to the follow-up about ws-OTHER.
    struct ReAddThisRun {
        calls: std::cell::Cell<u32>,
    }
    impl CreditDecider for ReAddThisRun {
        fn decide(&self, seats: &[(String, Option<chrono::DateTime<chrono::Utc>>)]) -> CreditChoice {
            self.calls.set(self.calls.get() + 1);
            assert!(!seats.is_empty());
            if self.calls.get() == 1 {
                let _lock = seat::CodexLock::try_acquire().unwrap().expect("lock is free");
                let mut cfg = SeatConfig::load().unwrap().unwrap();
                for s in &mut cfg.seats {
                    s.account_id = Some("ws-OTHER".into());
                }
                cfg.save().unwrap();
                CreditChoice::ThisRun
            } else {
                CreditChoice::Wait
            }
        }
    }
    let d = ReAddThisRun { calls: std::cell::Cell::new(0) };
    let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        panic!("this-run consent was for ws-1, not ws-OTHER")
    };
    // Consent for ws-1 does not cover ws-OTHER: the re-entry asks again, the
    // answer is wait, and the run stops at 77 without spending.
    assert_eq!(run_deps(attempt, &d), EXIT_CREDITS_CONSENT_NEEDED);
    assert_eq!(d.calls.get(), 2, "the new workspace was asked about separately");
}


// ===========================================================================
// Free usage-limit resets, and session cost
// ===========================================================================

use codex_clean::runner::EXIT_RESET_AVAILABLE;
use codex_clean::seat::{ResetPolicy, UsageResets};
use codex_clean::usage::{AccountUsage, ResetCredit, ResetListing, ResetOutcome, ThreadCost};

fn with_resets(mut snap: UsageSnapshot, available: u32) -> UsageSnapshot {
    snap.resets = Some(UsageResets {
        available,
        next_expires_at: Some(chrono::Utc::now() + chrono::Duration::days(9)),
        next_title: Some("Full reset (Weekly + 5 hr)".into()),
    });
    snap
}

/// Seats blocked the way the real account was: weekly at 100%, credits
/// available, and free resets granted.
fn setup_blocked_with_resets(env: &TestEnv, credits: CreditPolicy, resets: ResetPolicy) {
    setup_both_on_credits(env, credits);
    let mut cfg = SeatConfig::load().unwrap().unwrap();
    cfg.rotation.resets = resets;
    env.save_config(&cfg);
    let mut st = env.load_state();
    for n in ["main", "backup1"] {
        st.entry_mut(n).usage = Some(with_resets(on_credits_snapshot(), 3));
    }
    env.save_state(&st);
}

/// Client that scripts the reset/cost calls and counts them.
struct ResetClient {
    outcome: ResetOutcome,
    after: UsageSnapshot,
    consumes: std::sync::atomic::AtomicUsize,
    fetches: std::sync::atomic::AtomicUsize,
    fail: bool,
}

impl ResetClient {
    fn new(outcome: ResetOutcome, after: UsageSnapshot) -> Self {
        Self {
            outcome,
            after,
            consumes: std::sync::atomic::AtomicUsize::new(0),
            fetches: std::sync::atomic::AtomicUsize::new(0),
            fail: false,
        }
    }
    fn consumed(&self) -> usize {
        self.consumes.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl UsageClient for ResetClient {
    fn fetch(&self, _: &SE) -> Result<UsageSnapshot, UsageFetchError> {
        self.fetches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut s = self.after.clone();
        s.fetched_at = chrono::Utc::now();
        Ok(s)
    }
    fn consume_reset(&self, _: &SE, _: Option<&str>) -> Result<ResetOutcome, UsageFetchError> {
        self.consumes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail {
            return Err(UsageFetchError::Timeout(std::time::Duration::from_secs(1)));
        }
        Ok(self.outcome)
    }
    fn thread_cost(&self, _: &SE, _: &str) -> Result<ThreadCost, UsageFetchError> {
        Ok(ThreadCost { credits_micros: 420_000, usd_micros: Some(52_000) })
    }
    fn account_usage(&self, _: &SE) -> Result<AccountUsage, UsageFetchError> {
        Ok(AccountUsage { lifetime_tokens: Some(1_000_000), last_7d_tokens: 250_000 })
    }
    fn list_resets(&self, _: &SE) -> Result<ResetListing, UsageFetchError> {
        Ok(ResetListing {
            available: 3,
            credits: vec![ResetCredit {
                id: "RateLimitResetCredit_abc".into(),
                title: Some("Full reset (Weekly + 5 hr)".into()),
                expires_at: Some(chrono::Utc::now() + chrono::Duration::days(9)),
            }],
        })
    }
}

#[test]
fn a_free_reset_is_offered_before_paying_and_before_waiting() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_blocked_with_resets(&env, CreditPolicy::Ask, ResetPolicy::Ask);
    let client = ResetClient::new(ResetOutcome::Reset, snapshot(1, 1));
    let decider = decider(CreditChoice::ThisRun);
    let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        panic!("nothing should run: the free reset is offered first")
    };
    let exit = runner::run_codex_with_deps(&[], "hi", Mode::Exec, attempt, &client, &decider).unwrap();
    assert_eq!(exit, EXIT_RESET_AVAILABLE, "78 takes precedence over 77");
    assert_eq!(decider.calls.get(), 0, "the credits question is never asked");
    assert_eq!(client.consumed(), 0, "ask never redeems by itself");
    let log = fs::read_to_string(env.clean_home_path.join("seat-events.log")).unwrap();
    assert!(log.contains("reset_available"), "{}", log);
}

#[test]
fn never_policy_keeps_the_old_exit_codes_and_says_nothing_about_resets() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_blocked_with_resets(&env, CreditPolicy::Ask, ResetPolicy::Never);
    let client = ResetClient::new(ResetOutcome::Reset, snapshot(1, 1));
    let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        panic!("must not run")
    };
    let exit = runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &client).unwrap();
    assert_eq!(exit, EXIT_CREDITS_CONSENT_NEEDED, "credits question as before");
    assert_eq!(client.consumed(), 0);
    let st = env.load_state();
    assert!(st.get("main").usage.unwrap().resets.is_some(), "still recorded, just not offered");
}

#[test]
fn auto_redeems_once_and_the_recovered_seat_runs_even_at_the_end_of_the_budget() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_blocked_with_resets(&env, CreditPolicy::Ask, ResetPolicy::Auto);
    // Both seats already tried and cooling: the budget is spent.
    let mut st = env.load_state();
    for n in ["main", "backup1"] {
        let e = st.entry_mut(n);
        e.cooldown_until = Some(chrono::Utc::now() + chrono::Duration::hours(20));
        e.cooldown_reason = Some("rate_limit".into());
        e.usage_checked_at = Some(chrono::Utc::now());
    }
    env.save_state(&st);
    let client = ResetClient::new(ResetOutcome::Reset, with_resets(snapshot(2, 2), 2));
    let codex_home = env.codex_home_path.clone();
    let exit = runner::run_codex_with_client(&[], "hi", Mode::Exec, mock_attempt(&codex_home, |_| ok_attempt()), &client).unwrap();
    assert_eq!(exit, 0, "the reset recovered a seat and the run went through");
    assert_eq!(client.consumed(), 1, "exactly one grant spent");
    let st = env.load_state();
    let recovered = ["main", "backup1"].iter().filter(|n| st.get(n).cooldown_until.is_none()).count();
    assert_eq!(recovered, 1, "only the reset seat was cleared");
    let log = fs::read_to_string(env.clean_home_path.join("seat-events.log")).unwrap();
    assert!(log.contains("reset seat=") && log.contains("redeemed a free usage-limit reset"), "{}", log);
}

#[test]
fn auto_falls_through_when_the_backend_declines_or_errors() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for (outcome, fail) in [(ResetOutcome::NothingToReset, false), (ResetOutcome::NoCredit, false), (ResetOutcome::Reset, true)] {
        let env = TestEnv::new();
        setup_blocked_with_resets(&env, CreditPolicy::Ask, ResetPolicy::Auto);
        let mut client = ResetClient::new(outcome, snapshot(1, 1));
        client.fail = fail;
        let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
            panic!("must not run")
        };
        let exit = runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &client).unwrap();
        assert_eq!(exit, EXIT_CREDITS_CONSENT_NEEDED, "{:?}/{}: falls through to the credits path", outcome, fail);
        assert_eq!(client.consumed(), 1, "one attempt, not a loop");
    }
}

#[test]
fn a_reset_is_never_redeemed_for_a_block_it_cannot_lift() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for (reason, needs_login) in [("credits", false), ("spend_control", false), ("rate_limit", true)] {
        let env = TestEnv::new();
        setup_blocked_with_resets(&env, CreditPolicy::Never, ResetPolicy::Auto);
        let mut st = env.load_state();
        for n in ["main", "backup1"] {
            let e = st.entry_mut(n);
            e.cooldown_until = Some(chrono::Utc::now() + chrono::Duration::hours(20));
            e.cooldown_reason = Some(reason.into());
            e.needs_login = needs_login;
            e.usage_checked_at = Some(chrono::Utc::now());
        }
        env.save_state(&st);
        let client = ResetClient::new(ResetOutcome::Reset, snapshot(1, 1));
        let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
            panic!("must not run")
        };
        let exit = runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &client).unwrap();
        assert_eq!(client.consumed(), 0, "{}: a reset cannot lift this block", reason);
        assert!(exit == 75 || exit == 1, "{}: got {}", reason, exit);
    }
}

#[test]
fn a_stale_pre_upgrade_snapshot_is_refreshed_before_the_78_77_decision() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_both_on_credits(&env, CreditPolicy::Ask);
    // 0.8.0 wrote snapshots with no `resets` field at all.
    let mut st = env.load_state();
    for n in ["main", "backup1"] {
        let mut old = on_credits_snapshot();
        old.fetched_at = chrono::Utc::now() - chrono::Duration::hours(2);
        old.resets = None;
        st.entry_mut(n).usage = Some(old);
        st.entry_mut(n).cooldown_until = Some(chrono::Utc::now() + chrono::Duration::hours(5));
        st.entry_mut(n).cooldown_reason = Some("rate_limit".into());
    }
    env.save_state(&st);
    let client = ResetClient::new(ResetOutcome::Reset, with_resets(on_credits_snapshot(), 3));
    let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        panic!("must not run under ask")
    };
    let exit = runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &client).unwrap();
    assert_eq!(exit, EXIT_RESET_AVAILABLE, "the probe found the grants first");
    assert!(client.fetches.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    assert!(env.load_state().get("main").usage.unwrap().resets.is_some());
}

#[test]
fn seat_reset_command_outcomes_and_dry_run() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Successful redeem clears the cooldown.
    let env = TestEnv::new();
    setup_blocked_with_resets(&env, CreditPolicy::Ask, ResetPolicy::Ask);
    let mut st = env.load_state();
    st.entry_mut("main").cooldown_until = Some(chrono::Utc::now() + chrono::Duration::hours(20));
    st.entry_mut("main").cooldown_reason = Some("rate_limit".into());
    env.save_state(&st);
    let client = ResetClient::new(ResetOutcome::Reset, with_resets(snapshot(1, 1), 2));
    assert_eq!(codex_clean::seat_cmd::reset_with(&client, Some("main"), None, false, false).unwrap(), 0);
    assert_eq!(client.consumed(), 1);
    assert!(env.load_state().get("main").cooldown_until.is_none(), "cooldown cleared after the reset");

    // --dry-run lists and redeems nothing.
    let client = ResetClient::new(ResetOutcome::Reset, snapshot(1, 1));
    assert_eq!(codex_clean::seat_cmd::reset_with(&client, Some("main"), None, true, true).unwrap(), 0);
    assert_eq!(client.consumed(), 0, "--dry-run must never redeem");

    // nothingToReset / noCredit report and exit 1 without changing state.
    for outcome in [ResetOutcome::NothingToReset, ResetOutcome::NoCredit] {
        let env = TestEnv::new();
        setup_blocked_with_resets(&env, CreditPolicy::Ask, ResetPolicy::Ask);
        let mut st = env.load_state();
        st.entry_mut("main").cooldown_until = Some(chrono::Utc::now() + chrono::Duration::hours(20));
        st.entry_mut("main").cooldown_reason = Some("rate_limit".into());
        env.save_state(&st);
        let client = ResetClient::new(outcome, snapshot(1, 1));
        assert_eq!(codex_clean::seat_cmd::reset_with(&client, Some("main"), None, false, false).unwrap(), 1);
        assert!(env.load_state().get("main").cooldown_until.is_some(), "{:?}", outcome);
    }

    // Unknown seat is rejected before any call.
    let client = ResetClient::new(ResetOutcome::Reset, snapshot(1, 1));
    assert!(codex_clean::seat_cmd::reset_with(&client, Some("nope"), None, false, false).is_err());
    assert_eq!(client.consumed(), 0);
}

#[test]
fn cost_command_resolves_the_session_and_seat() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    env.write_seat("b", "acc-b");
    env.save_config(&cfg_with_seats(&[("a", "acc-a"), ("b", "acc-b")]));
    let client = ResetClient::new(ResetOutcome::Reset, snapshot(1, 1));
    // Nothing recorded yet.
    assert!(codex_clean::seat_cmd::cost_with(&client, None, true, None, false).is_err());
    // A run records the session on the seat that ran it.
    let codex_home = env.codex_home_path.clone();
    let attempt = move |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        let mut ok = ok_attempt();
        ok.output.session_id = Some("thread-xyz".into());
        let _ = fs::read(codex_home.join("auth.json"))?;
        Ok(ok)
    };
    assert_eq!(runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &client).unwrap(), 0);
    let st = env.load_state();
    let ran = st.active_seat.clone().unwrap();
    assert_eq!(st.get(&ran).last_session.as_deref(), Some("thread-xyz"));
    assert_eq!(codex_clean::seat_cmd::cost_with(&client, None, true, None, true).unwrap(), 0);
    assert_eq!(codex_clean::seat_cmd::cost_with(&client, Some("thread-xyz"), false, None, false).unwrap(), 0);
    // Neither an id nor --last is an error, not a guess.
    assert!(codex_clean::seat_cmd::cost_with(&client, None, false, None, false).is_err());
}

#[test]
fn session_cost_appears_on_the_seat_line_only_when_enabled() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    let mut cfg = cfg_with_seats(&[("a", "acc-a")]);
    cfg.rotation.show_session_cost = true;
    env.save_config(&cfg);
    let client = ResetClient::new(ResetOutcome::Reset, snapshot(1, 1));
    let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        let mut ok = ok_attempt();
        ok.output.session_id = Some("t-1".into());
        Ok(ok)
    };
    assert_eq!(runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &client).unwrap(), 0);
    // A client without cost support leaves the line alone (no failure).
    let plain = FakeClient { by_seat: Box::new(|_| Ok(snapshot(1, 1))), rewrite_slot_tag: None };
    assert_eq!(runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &plain).unwrap(), 0);
}

#[cfg(unix)]
#[test]
fn stdout_tells_a_headless_caller_about_the_free_reset() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_blocked_with_resets(&env, CreditPolicy::Ask, ResetPolicy::Ask);
    let bin = tempfile::tempdir().unwrap();
    install_fake_codex_exec(bin.path(), "{\"type\":\"thread.started\",\"thread_id\":\"t\"}", 0);
    let (code, out) = run_binary(&env, bin.path(), &[]);
    assert_eq!(code, 78, "{}", out);
    let last = out.lines().last().unwrap_or_default();
    assert!(last.contains("3 free usage-limit reset(s) available on main"), "{}", last);
    assert!(last.contains("codex-clean seat reset main"), "{}", last);
    assert!(last.contains("next expires"), "{}", last);
    // Why the run stopped, and both ways out — above the `Seats:` line, which
    // stays last because parsers treat it as the status paragraph.
    let stopped = out
        .lines()
        .find(|l| l.starts_with("Stopped for a free reset"))
        .unwrap_or_else(|| panic!("no stopped line in: {}", out));
    assert!(stopped.contains("exit 78"), "{}", stopped);
    assert!(stopped.contains("codex-clean seat reset main"), "{}", stopped);
    assert!(
        stopped.contains("CODEX_CLEAN_USE_CREDITS=1"),
        "credits are available here, so the paid way out must be named: {}",
        stopped
    );
}

#[test]
fn the_stopped_line_names_credits_only_when_they_could_help() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Cooling on a plain rate limit with no credits: a reset lifts it, money
    // does not, so pointing at CODEX_CLEAN_USE_CREDITS=1 would be a dead end.
    let env = TestEnv::new();
    setup_blocked_with_resets(&env, CreditPolicy::Ask, ResetPolicy::Ask);
    let mut st = env.load_state();
    for n in ["main", "backup1"] {
        let e = st.entry_mut(n);
        e.usage = Some(with_resets(snapshot(100, 100), 3));
        e.cooldown_until = Some(chrono::Utc::now() + chrono::Duration::hours(2));
        e.cooldown_reason = Some("rate_limit".into());
    }
    env.save_state(&st);
    let bin = tempfile::tempdir().unwrap();
    install_fake_codex_exec(bin.path(), "{\"type\":\"thread.started\",\"thread_id\":\"t\"}", 0);
    let (code, out) = run_binary(&env, bin.path(), &[]);
    assert_eq!(code, 78, "{}", out);
    let stopped = out
        .lines()
        .find(|l| l.starts_with("Stopped for a free reset"))
        .unwrap_or_else(|| panic!("no stopped line in: {}", out));
    assert!(
        !stopped.contains("CODEX_CLEAN_USE_CREDITS"),
        "credits cannot lift a rate-limit cooldown: {}",
        stopped
    );
}


#[test]
fn the_reset_decision_still_happens_after_the_last_failed_attempt() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Both seats look healthy, so the run spends its whole budget before
    // anything is known to be blocked; the reset offer must still appear.
    let env = TestEnv::new();
    setup_blocked_with_resets(&env, CreditPolicy::Never, ResetPolicy::Ask);
    let mut st = env.load_state();
    for n in ["main", "backup1"] {
        st.entry_mut(n).usage = Some(with_resets(snapshot(10, 10), 3));
    }
    env.save_state(&st);
    let client = ResetClient::new(ResetOutcome::Reset, with_resets(snapshot(1, 1), 2));
    let codex_home = env.codex_home_path.clone();
    let attempt = mock_attempt(&codex_home, |_| rate_limit_attempt());
    let exit = runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &client).unwrap();
    assert_eq!(exit, EXIT_RESET_AVAILABLE, "78 after the budget is spent, not 75");
    assert_eq!(client.consumed(), 0, "ask does not redeem");

    // With auto, the same situation redeems and runs the recovered seat.
    let env = TestEnv::new();
    setup_blocked_with_resets(&env, CreditPolicy::Never, ResetPolicy::Auto);
    let mut st = env.load_state();
    for n in ["main", "backup1"] {
        st.entry_mut(n).usage = Some(with_resets(snapshot(10, 10), 3));
    }
    env.save_state(&st);
    let client = ResetClient::new(ResetOutcome::Reset, with_resets(snapshot(1, 1), 2));
    let codex_home = env.codex_home_path.clone();
    let calls = std::rc::Rc::new(RefCell::new(0usize));
    let calls2 = calls.clone();
    let attempt = move |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        *calls2.borrow_mut() += 1;
        let _ = fs::read(codex_home.join("auth.json"))?;
        Ok(if *calls2.borrow() <= 2 { rate_limit_attempt() } else { ok_attempt() })
    };
    let exit = runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &client).unwrap();
    assert_eq!(exit, 0, "the reset bought one more attempt");
    assert_eq!(*calls.borrow(), 3, "two failures, then the recovered seat");
    assert_eq!(client.consumed(), 1);
}

#[test]
fn a_pinned_seat_is_offered_a_reset_too() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_blocked_with_resets(&env, CreditPolicy::Never, ResetPolicy::Ask);
    let mut st = env.load_state();
    st.entry_mut("main").usage = Some(with_resets(snapshot(10, 10), 3));
    env.save_state(&st);
    std::env::set_var("CODEX_CLEAN_SEAT", "main");
    let client = ResetClient::new(ResetOutcome::Reset, snapshot(1, 1));
    let codex_home = env.codex_home_path.clone();
    let attempt = mock_attempt(&codex_home, |_| rate_limit_attempt());
    let exit = runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &client).unwrap();
    assert_eq!(exit, EXIT_RESET_AVAILABLE, "a pin does not hide the free option");
    std::env::remove_var("CODEX_CLEAN_SEAT");
}

#[test]
fn a_non_active_seat_refresh_never_takes_over_the_global_auth_file() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("main", "acc-main");
    env.write_seat("backup1", "acc-backup1");
    env.save_config(&cfg_with_seats(&[("main", "acc-main"), ("backup1", "acc-backup1")]));
    let state = SeatState { active_seat: Some("main".to_string()), ..Default::default() };
    env.save_state(&state);
    fs::write(env.codex_home_path.join("auth.json"), fake_auth_json("acc-main")).unwrap();

    // backup1's slot changes (as an app-server call would rotate it).
    let before = seat::slot_snapshot("backup1");
    fs::write(
        env.clean_home_path.join("seats/backup1/auth.json"),
        fake_auth_json_refreshed("acc-backup1", "rotated"),
    )
    .unwrap();
    assert!(!seat::sync_active_auth("backup1", before), "not the active seat");
    assert_eq!(env.active_auth_account_id().as_deref(), Some("acc-main"), "global auth untouched");

    // The active seat's rotation is still mirrored.
    let before = seat::slot_snapshot("main");
    fs::write(
        env.clean_home_path.join("seats/main/auth.json"),
        fake_auth_json_refreshed("acc-main", "rotated"),
    )
    .unwrap();
    assert!(seat::sync_active_auth("main", before));
    assert!(fs::read_to_string(env.codex_home_path.join("auth.json")).unwrap().contains("fake-access-rotated"));
}

#[test]
fn a_spent_reset_frees_the_seat_even_when_the_follow_up_read_fails() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_blocked_with_resets(&env, CreditPolicy::Never, ResetPolicy::Auto);
    let mut st = env.load_state();
    for n in ["main", "backup1"] {
        let e = st.entry_mut(n);
        e.cooldown_until = Some(chrono::Utc::now() + chrono::Duration::hours(20));
        e.cooldown_reason = Some("rate_limit".into());
        e.usage_checked_at = Some(chrono::Utc::now());
    }
    env.save_state(&st);
    /// Consume succeeds; the follow-up usage read always fails.
    struct FlakyClient {
        consumes: std::sync::atomic::AtomicUsize,
    }
    impl UsageClient for FlakyClient {
        fn fetch(&self, _: &SE) -> Result<UsageSnapshot, UsageFetchError> {
            Err(UsageFetchError::Timeout(std::time::Duration::from_secs(1)))
        }
        fn consume_reset(&self, _: &SE, _: Option<&str>) -> Result<ResetOutcome, UsageFetchError> {
            self.consumes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ResetOutcome::Reset)
        }
    }
    let client = FlakyClient { consumes: std::sync::atomic::AtomicUsize::new(0) };
    let codex_home = env.codex_home_path.clone();
    let exit = runner::run_codex_with_client(&[], "hi", Mode::Exec, mock_attempt(&codex_home, |_| ok_attempt()), &client).unwrap();
    assert_eq!(exit, 0, "the grant was spent, so the seat must actually be used");
    assert_eq!(client.consumes.load(std::sync::atomic::Ordering::SeqCst), 1);
    let st = env.load_state();
    let freed = ["main", "backup1"].iter().filter(|n| st.get(n).cooldown_until.is_none()).count();
    assert_eq!(freed, 1, "the reset seat was freed despite the failed re-read");
}

#[test]
fn the_stdout_advice_never_points_at_a_block_a_reset_cannot_lift() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_blocked_with_resets(&env, CreditPolicy::Never, ResetPolicy::Ask);
    let mut st = env.load_state();
    for n in ["main", "backup1"] {
        let e = st.entry_mut(n);
        e.cooldown_until = Some(chrono::Utc::now() + chrono::Duration::hours(20));
        e.cooldown_reason = Some("spend_control".into());
    }
    env.save_state(&st);
    let cfg = SeatConfig::load().unwrap().unwrap();
    let notice = seat::seat_notice(&cfg, &env.load_state(), chrono::Utc::now(), &|_| false).unwrap();
    assert!(!notice.contains("seat reset"), "a spend cap is not lifted by a reset: {}", notice);

    // A window cooldown is, so there the advice appears.
    let mut st = env.load_state();
    for n in ["main", "backup1"] {
        st.entry_mut(n).cooldown_reason = Some("rate_limit".into());
    }
    env.save_state(&st);
    let notice = seat::seat_notice(&cfg, &env.load_state(), chrono::Utc::now(), &|_| false).unwrap();
    assert!(notice.contains("codex-clean seat reset main"), "{}", notice);
}

#[test]
fn cost_last_follows_the_session_clock_not_the_run_clock() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    env.write_seat("a", "acc-a");
    env.write_seat("b", "acc-b");
    env.save_config(&cfg_with_seats(&[("a", "acc-a"), ("b", "acc-b")]));
    let mut st = SeatState::default();
    let now = chrono::Utc::now();
    // b ran the most recent session; a ran later but failed, keeping an old id.
    let e = st.entry_mut("b");
    e.last_session = Some("newest".into());
    e.last_session_at = Some(now - chrono::Duration::minutes(1));
    e.last_used = Some(now - chrono::Duration::minutes(1));
    let e = st.entry_mut("a");
    e.last_session = Some("older".into());
    e.last_session_at = Some(now - chrono::Duration::hours(3));
    e.last_used = Some(now); // the later, failed run
    env.save_state(&st);

    struct RecordingClient {
        seen: std::sync::Mutex<Vec<(String, String)>>,
    }
    impl UsageClient for RecordingClient {
        fn fetch(&self, _: &SE) -> Result<UsageSnapshot, UsageFetchError> {
            Err(UsageFetchError::Protocol("unused".into()))
        }
        fn thread_cost(&self, seat: &SE, thread: &str) -> Result<ThreadCost, UsageFetchError> {
            self.seen.lock().unwrap().push((seat.name.clone(), thread.to_string()));
            Ok(ThreadCost { credits_micros: 1, usd_micros: None })
        }
    }
    let client = RecordingClient { seen: std::sync::Mutex::new(Vec::new()) };
    assert_eq!(codex_clean::seat_cmd::cost_with(&client, None, true, None, false).unwrap(), 0);
    assert_eq!(
        client.seen.lock().unwrap().as_slice(),
        [("b".to_string(), "newest".to_string())],
        "the most recently recorded session wins"
    );
}


#[test]
fn consent_to_pay_is_honoured_while_a_free_reset_is_pending() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Credits are permitted (always), so the seat is runnable. Under `ask` the
    // run must PROCEED: stopping a run the user has already consented to pay
    // for leaves a headless caller no way to act on its own approval — it
    // re-runs with consent, gets 78 again, and loops. The free reset is still
    // advertised on the `Seats:` line.
    let env = TestEnv::new();
    setup_blocked_with_resets(&env, CreditPolicy::Always, ResetPolicy::Ask);
    let client = ResetClient::new(ResetOutcome::Reset, snapshot(1, 1));
    let codex_home = env.codex_home_path.clone();
    assert_eq!(
        runner::run_codex_with_client(&[], "hi", Mode::Exec, mock_attempt(&codex_home, |_| ok_attempt()), &client).unwrap(),
        0
    );
    assert_eq!(client.consumed(), 0, "ask must never redeem by itself");

    // With auto it redeems instead of paying, and then runs.
    let env = TestEnv::new();
    setup_blocked_with_resets(&env, CreditPolicy::Always, ResetPolicy::Auto);
    let client = ResetClient::new(ResetOutcome::Reset, with_resets(snapshot(2, 2), 2));
    let codex_home = env.codex_home_path.clone();
    assert_eq!(
        runner::run_codex_with_client(&[], "hi", Mode::Exec, mock_attempt(&codex_home, |_| ok_attempt()), &client).unwrap(),
        0
    );
    assert_eq!(client.consumed(), 1);
    let _ = env;
}

#[test]
fn every_consent_route_proceeds_and_still_advertises_the_free_reset() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Route 1: the per-run env var. This is the exact loop that was reported —
    // 78, user approves credits, re-run with the var, 78 again.
    let env = TestEnv::new();
    setup_blocked_with_resets(&env, CreditPolicy::Ask, ResetPolicy::Ask);
    std::env::set_var("CODEX_CLEAN_USE_CREDITS", "1");
    let client = ResetClient::new(ResetOutcome::Reset, snapshot(1, 1));
    let codex_home = env.codex_home_path.clone();
    assert_eq!(
        runner::run_codex_with_client(&[], "hi", Mode::Exec, mock_attempt(&codex_home, |_| ok_attempt()), &client).unwrap(),
        0,
        "consent given by env var must let the run proceed, not return 78 again"
    );
    assert_eq!(client.consumed(), 0, "the grant is not spent on the user's behalf");
    // The free option is still put in front of the caller, as advice.
    let cfg = SeatConfig::load().unwrap().unwrap();
    let notice = seat::seat_notice(&cfg, &env.load_state(), chrono::Utc::now(), &|_| true).unwrap();
    assert!(notice.contains("codex-clean seat reset main"), "{}", notice);
    std::env::remove_var("CODEX_CLEAN_USE_CREDITS");

    // Route 2: a `seat credits allow` grant recorded in state.
    let env = TestEnv::new();
    setup_blocked_with_resets(&env, CreditPolicy::Ask, ResetPolicy::Ask);
    let cfg = SeatConfig::load().unwrap().unwrap();
    let mut st = env.load_state();
    let resets_at = chrono::Utc::now() + chrono::Duration::days(3);
    seat::grant_credits_until_reset(
        &cfg,
        &mut st,
        &[("main".to_string(), Some(resets_at)), ("backup1".to_string(), Some(resets_at))],
        chrono::Utc::now(),
    )
    .unwrap();
    env.save_state(&st);
    let client = ResetClient::new(ResetOutcome::Reset, snapshot(1, 1));
    let codex_home = env.codex_home_path.clone();
    assert_eq!(
        runner::run_codex_with_client(&[], "hi", Mode::Exec, mock_attempt(&codex_home, |_| ok_attempt()), &client).unwrap(),
        0,
        "a standing grant is consent too"
    );
    assert_eq!(client.consumed(), 0);
    let _ = env;
}

#[test]
fn a_consented_seat_that_cannot_be_attempted_still_offers_the_reset() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // The budget is one attempt. An in-quota seat takes it and hits a
    // resettable rate limit; the remaining seat is on credits with standing
    // consent, so the pick SUCCEEDS — but it can never be attempted. The run
    // cannot proceed, so the free reset must still be offered rather than the
    // failed child's exit code returned.
    let env = TestEnv::new();
    setup_blocked_with_resets(&env, CreditPolicy::Always, ResetPolicy::Ask);
    let mut cfg = SeatConfig::load().unwrap().unwrap();
    cfg.rotation.max_retries = 0;
    env.save_config(&cfg);
    let mut st = env.load_state();
    // main is in quota and least recently used, so it runs first and fails.
    st.entry_mut("main").usage = Some(with_resets(snapshot(10, 10), 3));
    st.entry_mut("main").last_used = None;
    st.entry_mut("backup1").last_used = Some(chrono::Utc::now());
    env.save_state(&st);

    let client = ResetClient::new(ResetOutcome::Reset, snapshot(1, 1));
    let codex_home = env.codex_home_path.clone();
    let attempt = mock_attempt(&codex_home, |acct| {
        if acct == "user-bob" {
            panic!("the paid seat has no budget left; it must not run");
        }
        rate_limit_attempt()
    });
    assert_eq!(
        runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &client).unwrap(),
        EXIT_RESET_AVAILABLE,
        "a run that cannot proceed must offer the free reset, consent or not"
    );
    assert_eq!(client.consumed(), 0, "ask still does not redeem by itself");
    let _ = env;
}

#[test]
fn the_stopped_line_offers_a_re_run_when_consent_is_already_held() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Budget exhausted with consent already given: telling the caller to set
    // CODEX_CLEAN_USE_CREDITS=1 would be useless — it is already effectively
    // set. What unblocks the seat is a fresh invocation.
    let env = TestEnv::new();
    setup_blocked_with_resets(&env, CreditPolicy::Always, ResetPolicy::Ask);
    let mut cfg = SeatConfig::load().unwrap().unwrap();
    cfg.rotation.max_retries = 0;
    env.save_config(&cfg);
    let mut st = env.load_state();
    st.entry_mut("main").usage = Some(with_resets(snapshot(10, 10), 3));
    st.entry_mut("main").last_used = None;
    st.entry_mut("backup1").last_used = Some(chrono::Utc::now());
    env.save_state(&st);

    let bin = tempfile::tempdir().unwrap();
    install_fake_codex_exec(bin.path(), "{\"type\":\"error\",\"message\":\"You've hit your usage limit.\"}", 1);
    let (code, out) = run_binary(&env, bin.path(), &[]);
    assert_eq!(code, 78, "{}", out);
    let stopped = out
        .lines()
        .find(|l| l.starts_with("Stopped for a free reset"))
        .unwrap_or_else(|| panic!("no stopped line in: {}", out));
    assert!(stopped.contains("re-run to give the already-consented seat another attempt"), "{}", stopped);
    assert!(!stopped.contains("CODEX_CLEAN_USE_CREDITS"), "consent is already held: {}", stopped);
}

#[test]
fn seat_reset_policy_shows_sets_and_rejects() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_blocked_with_resets(&env, CreditPolicy::Ask, ResetPolicy::Ask);
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_codex-clean"))
            .args(args)
            .env("CODEX_CLEAN_HOME", &env.clean_home_path)
            .env("CODEX_HOME", &env.codex_home_path)
            .output()
            .unwrap()
    };

    let out = run(&["seat", "reset-policy"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("ask"), "{}", text);
    assert!(text.contains("Available: ask (default), never, auto"), "{}", text);

    for value in ["never", "auto", "ask"] {
        let out = run(&["seat", "reset-policy", value]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(
            SeatConfig::load().unwrap().unwrap().rotation.resets,
            ResetPolicy::parse(value).unwrap(),
            "{} did not round-trip through seats.toml",
            value
        );
    }

    let out = run(&["seat", "reset-policy", "sometimes"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unknown reset policy"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A headless caller can read the policy it is subject to.
    let out = run(&["seat", "status", "--json"]);
    let text = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {}", e, text));
    assert_eq!(v["resets_mode"], "ask");
}

#[test]
fn a_per_model_cap_is_not_treated_as_resettable() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_blocked_with_resets(&env, CreditPolicy::Never, ResetPolicy::Auto);
    let mut st = env.load_state();
    for n in ["main", "backup1"] {
        let e = st.entry_mut(n);
        e.cooldown_until = Some(chrono::Utc::now() + chrono::Duration::hours(4));
        e.cooldown_reason = Some("model_limit".into());
        e.usage_checked_at = Some(chrono::Utc::now());
        e.usage = Some(with_resets(snapshot(10, 10), 3));
    }
    env.save_state(&st);
    let client = ResetClient::new(ResetOutcome::Reset, snapshot(1, 1));
    let attempt = |_a: &[String], _p: &str, _m: &Mode, _s: bool| -> anyhow::Result<AttemptResult> {
        panic!("must not run")
    };
    let exit = runner::run_codex_with_client(&[], "hi", Mode::Exec, attempt, &client).unwrap();
    assert_eq!(client.consumed(), 0, "a weekly/5h reset does not lift a per-model cap");
    assert_eq!(exit, 75);
    let cfg = SeatConfig::load().unwrap().unwrap();
    let notice = seat::seat_notice(&cfg, &env.load_state(), chrono::Utc::now(), &|_| false).unwrap();
    assert!(!notice.contains("seat reset"), "{}", notice);
}

#[test]
fn seat_reset_without_a_name_picks_the_seat_that_is_actually_blocked() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let env = TestEnv::new();
    setup_blocked_with_resets(&env, CreditPolicy::Never, ResetPolicy::Ask);
    // Only backup1 is blocked in a way a reset lifts; main is fine.
    let mut st = env.load_state();
    st.entry_mut("main").usage = Some(with_resets(snapshot(5, 5), 3));
    let e = st.entry_mut("backup1");
    e.usage = Some(with_resets(snapshot(0, 100), 3));
    e.cooldown_until = Some(chrono::Utc::now() + chrono::Duration::hours(9));
    e.cooldown_reason = Some("rate_limit".into());
    st.active_seat = Some("main".into());
    env.save_state(&st);

    struct SeatRecorder {
        seen: std::sync::Mutex<Vec<String>>,
    }
    impl UsageClient for SeatRecorder {
        fn fetch(&self, _: &SE) -> Result<UsageSnapshot, UsageFetchError> {
            Ok(snapshot(1, 1))
        }
        fn consume_reset(&self, seat: &SE, _: Option<&str>) -> Result<ResetOutcome, UsageFetchError> {
            self.seen.lock().unwrap().push(seat.name.clone());
            Ok(ResetOutcome::Reset)
        }
    }
    let client = SeatRecorder { seen: std::sync::Mutex::new(Vec::new()) };
    assert_eq!(codex_clean::seat_cmd::reset_with(&client, None, None, false, false).unwrap(), 0);
    assert_eq!(client.seen.lock().unwrap().as_slice(), ["backup1".to_string()], "the blocked seat, not the active one");

    // A pin wins over everything.
    std::env::set_var("CODEX_CLEAN_SEAT", "main");
    let client = SeatRecorder { seen: std::sync::Mutex::new(Vec::new()) };
    assert_eq!(codex_clean::seat_cmd::reset_with(&client, None, None, false, false).unwrap(), 0);
    assert_eq!(client.seen.lock().unwrap().as_slice(), ["main".to_string()]);
    std::env::remove_var("CODEX_CLEAN_SEAT");
}
