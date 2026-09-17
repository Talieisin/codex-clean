use std::env;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};

use crate::events::{extract_event, Event};
use crate::output::CodexOutput;
use crate::ratelimit::{self, FailureKind};
use crate::seat::{
    self, cool_seats, log_event, log_excerpt, refresh_back_guarded, seat_notice, swap_active_auth,
    unmatched_log_path, warn_refresh_back, workspace_siblings, CodexLock, CreditPolicy,
    ResetPolicy, SeatConfig, SeatPickError, SeatState, Strategy, SCRUB_ENV_VARS,
};
use crate::usage::{self, quota_state, AppServerClient, QuotaState, ResetOutcome, UsageClient};

const STDERR_CAP_BYTES: usize = 10 * 1024 * 1024;

/// `EX_TEMPFAIL`: every seat is cooling; try again later.
pub const EXIT_ALL_SEATS_COOLING: i32 = 75;

/// `EX_NOPERM`: included quota is used up and only workspace codex credits
/// remain, which need the user's consent. Distinct from 75 so a
/// non-interactive caller (e.g. an agent) asks the user instead of retrying.
pub const EXIT_CREDITS_CONSENT_NEEDED: i32 = 77;

/// A free usage-limit reset would unblock this run, and `rotation.resets` is
/// `ask`. Offered ahead of 77 (which spends money) and 75 (which just waits),
/// so an agent can put the free option to the user first.
pub const EXIT_RESET_AVAILABLE: i32 = 78;

/// `(seat, reason)` pairs for seats exhausted earlier in a run.
type Exhausted = Vec<(String, String)>;

/// Target for resume command
pub enum ResumeTarget {
    /// Resume a specific session by ID
    SessionId(String),
    /// Resume the most recent session
    Last,
}

/// Execution mode for codex
pub enum Mode {
    /// Run a new exec session
    Exec,
    /// Resume an existing session
    Resume(ResumeTarget),
    /// Run a code review
    Review,
}

/// Result of a single codex invocation, captured but not yet printed.
pub struct AttemptResult {
    pub output: CodexOutput,
    pub stderr_buffer: Vec<u8>,
    pub stderr_truncated: bool,
    pub stderr_error: Option<io::Error>,
    /// Codex's exit code, escalated to 1 if codex exited 0 but emitted error events.
    pub exit_code: i32,
    pub status_success: bool,
    pub child_exit: i32,
}

/// Run codex with the given arguments and prompt. Drives the multi-seat
/// orchestration if seats are configured; otherwise behaves identically to
/// the pre-seat version. `interactive` says whether a credits prompt may be
/// shown (a terminal on stdin and stderr, prompt not read from stdin).
pub fn run_codex(args: &[String], prompt: &str, mode: Mode, interactive: bool) -> Result<i32> {
    let decider: &dyn CreditDecider = if interactive { &TerminalPrompt } else { &NoPrompt };
    run_codex_with_deps(args, prompt, mode, attempt_codex, &AppServerClient::default(), decider)
}

/// Non-interactive orchestration with an injected spawner and **no** usage
/// fetching (automatic checks fail fast instead of spawning codex). Tests.
pub fn run_codex_with<F>(args: &[String], prompt: &str, mode: Mode, attempt: F) -> Result<i32>
where
    F: Fn(&[String], &str, &Mode, bool) -> Result<AttemptResult>,
{
    run_codex_with_client(args, prompt, mode, attempt, &usage::NoUsageClient)
}

/// Non-interactive orchestration with an injected usage client (tests).
pub fn run_codex_with_client<F>(
    args: &[String],
    prompt: &str,
    mode: Mode,
    attempt: F,
    usage_client: &dyn UsageClient,
) -> Result<i32>
where
    F: Fn(&[String], &str, &Mode, bool) -> Result<AttemptResult>,
{
    run_codex_with_deps(args, prompt, mode, attempt, usage_client, &NoPrompt)
}

// ---------------------------------------------------------------------------
// Credit consent
// ---------------------------------------------------------------------------

/// Why a run is allowed to spend workspace credits. Decided when the seat is
/// picked and carried with the attempt, so the `Seat:` line reports the
/// consent that applied even if a grant expires mid-run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreditUse {
    /// `CODEX_CLEAN_USE_CREDITS=1`, or "use credits for this run" at the prompt.
    ThisRun,
    /// A "use credits until quota resets" grant for the seat's workspace.
    Grant(DateTime<Utc>),
    /// `rotation.credits = "always"`.
    Always,
}

impl CreditUse {
    pub fn describe(self) -> String {
        match self {
            Self::ThisRun => "this run".to_string(),
            Self::Grant(until) => format!("until {}", until.with_timezone(&Local).format("%a %H:%M")),
            Self::Always => "always".to_string(),
        }
    }
}

/// The consent (if any) this invocation has to spend credits on `seat`.
///
/// `this_run_workspaces` are the workspaces the user said "use credits for
/// this run" about at the prompt. That consent covers only those workspaces,
/// checked against the seat's *current* workspace, so a seat re-registered
/// under another workspace while the question was open is not covered.
/// `CODEX_CLEAN_USE_CREDITS=1` is invocation-wide by design.
pub fn consent_for(
    cfg: &SeatConfig,
    state: &SeatState,
    seat_name: &str,
    this_run_workspaces: &[String],
    now: DateTime<Utc>,
) -> Option<CreditUse> {
    if cfg.rotation.credits == CreditPolicy::Always {
        return Some(CreditUse::Always);
    }
    let workspace = seat::workspace_key(cfg, seat_name);
    if seat::env_use_credits() || this_run_workspaces.contains(&workspace) {
        return Some(CreditUse::ThisRun);
    }
    state.active_grant(&workspace, now).map(CreditUse::Grant)
}

/// The answer to "included quota is used up — wait or spend credits?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreditChoice {
    Wait,
    ThisRun,
    UntilReset,
    Always,
}

impl CreditChoice {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wait => "wait",
            Self::ThisRun => "this run",
            Self::UntilReset => "until quota resets",
            Self::Always => "always",
        }
    }
}

/// Parse a prompt answer. Anything but `o`, `u` or `a` (including empty
/// input, EOF and garbage) means wait: consent to spend money is never
/// inferred.
pub fn parse_credit_choice(answer: &str) -> CreditChoice {
    match answer.trim().to_ascii_lowercase().as_str() {
        "o" | "once" | "this run" => CreditChoice::ThisRun,
        "u" | "until" | "until reset" => CreditChoice::UntilReset,
        "a" | "always" => CreditChoice::Always,
        _ => CreditChoice::Wait,
    }
}

/// Decides whether to spend credits when every candidate seat has used its
/// included quota. Called with `CodexLock` **released**, so a slow answer
/// never blocks other codex-clean runs.
pub trait CreditDecider {
    fn decide(&self, seats: &[(String, Option<DateTime<Utc>>)]) -> CreditChoice;
}

/// Background / non-interactive: never spend without prior consent.
pub struct NoPrompt;

impl CreditDecider for NoPrompt {
    fn decide(&self, _: &[(String, Option<DateTime<Utc>>)]) -> CreditChoice {
        CreditChoice::Wait
    }
}

/// Ask on the terminal (stderr prompt, stdin answer).
pub struct TerminalPrompt;

impl CreditDecider for TerminalPrompt {
    fn decide(&self, seats: &[(String, Option<DateTime<Utc>>)]) -> CreditChoice {
        eprintln!(
            "Included quota is used up: {}.",
            seat::describe_quota_resets(seats)
        );
        eprintln!(
            "Workspace credits are available. (A run that starts on included quota can still finish on credits.)"
        );
        eprintln!("  [w] wait for quota (exit 77)   [o] use credits for this run");
        eprintln!("  [u] use credits until quota resets   [a] always use credits");
        eprint!("Choice [w]: ");
        let _ = io::stderr().flush();
        let mut line = String::new();
        match io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => CreditChoice::Wait,
            Ok(_) => parse_credit_choice(&line),
        }
    }
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// A failed attempt kept for printing: which seat, why it was cooled, what
/// had been exhausted before it, and the credit consent it ran under.
#[derive(Clone)]
struct FailedSeat {
    seat: String,
    reason: String,
    before: Exhausted,
    credit_use: Option<CreditUse>,
}

/// State that survives across orchestration passes within one invocation
/// (a pass ends when a credits decision is needed and the lock is released).
#[derive(Default)]
struct RunCtx {
    tried_seats: Vec<String>,
    attempts_used: u32,
    last_failure: Option<AttemptResult>,
    last_failed_seat: Option<FailedSeat>,
    exhausted_so_far: Exhausted,
    /// Workspaces the user gave "use credits for this run" consent for.
    credits_this_run: Vec<String>,
    /// Free resets redeemed in this invocation (at most one), kept across a
    /// credits-decision re-entry.
    resets_redeemed: u32,
    /// Extra attempts granted because a reset recovered a seat.
    extra_attempts: u32,
    balanced_refreshed: bool,
    blocked_probed: bool,
    prerun_checked: Vec<String>,
    decisions: u32,
}

/// One seat in a credits decision: which seat, its workspace at the time the
/// question was asked, and when its quota resets. The workspace is re-checked
/// under the lock before any grant is written, so consent given for one
/// workspace can never attach to another (e.g. a seat re-added meanwhile).
#[derive(Debug, Clone)]
struct PromptedSeat {
    seat: String,
    workspace: String,
    resets_at: Option<DateTime<Utc>>,
}

enum Step {
    Done(i32),
    NeedCreditDecision(Vec<PromptedSeat>),
}

/// Upper bound on credits decisions in one invocation (each can change
/// config/state and re-enter orchestration).
const MAX_CREDIT_DECISIONS: u32 = 3;

/// Full orchestration with injected codex spawner, usage client and credits
/// decider.
pub fn run_codex_with_deps<F>(
    args: &[String],
    prompt: &str,
    mode: Mode,
    attempt: F,
    usage_client: &dyn UsageClient,
    decider: &dyn CreditDecider,
) -> Result<i32>
where
    F: Fn(&[String], &str, &Mode, bool) -> Result<AttemptResult>,
{
    let cfg_opt = SeatConfig::load().context("loading seats.toml")?;
    if !matches!(&cfg_opt, Some(c) if !c.seats.is_empty()) {
        // Backwards-compat: no seats configured → run as today.
        let result = attempt(args, prompt, &mode, false)?;
        print_attempt(&result);
        return Ok(result.exit_code);
    }

    let override_seat = env::var("CODEX_CLEAN_SEAT").ok().filter(|s| !s.is_empty());
    let mut ctx = RunCtx::default();
    loop {
        let step = orchestrate(
            args,
            prompt,
            &mode,
            &attempt,
            usage_client,
            override_seat.as_deref(),
            &mut ctx,
        )?;
        let prompted = match step {
            Step::Done(code) => return Ok(code),
            Step::NeedCreditDecision(p) => p,
        };
        let seats: Vec<(String, Option<DateTime<Utc>>)> =
            prompted.iter().map(|p| (p.seat.clone(), p.resets_at)).collect();
        // CodexLock was released when `orchestrate` returned.
        let cfg = SeatConfig::load()?.unwrap_or_default();
        ctx.decisions += 1;
        let choice = if cfg.rotation.credits == CreditPolicy::Ask && ctx.decisions <= MAX_CREDIT_DECISIONS {
            decider.decide(&seats)
        } else {
            CreditChoice::Wait
        };
        let names: Vec<&str> = seats.iter().map(|(n, _)| n.as_str()).collect();
        log_event(
            "credits",
            &names.join(","),
            &format!("quota used up with credits available; choice={} (credits: {})", choice.as_str(), cfg.rotation.credits),
        );
        let applied = match choice {
            CreditChoice::Wait => false,
            CreditChoice::ThisRun => {
                for p in &prompted {
                    if !ctx.credits_this_run.contains(&p.workspace) {
                        ctx.credits_this_run.push(p.workspace.clone());
                    }
                }
                true
            }
            CreditChoice::UntilReset => match grant_until_reset(&prompted) {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("Could not record the credits grant ({:#}); not spending credits.", e);
                    false
                }
            },
            CreditChoice::Always => match set_credit_policy(CreditPolicy::Always) {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("Could not save credits = \"always\" ({:#}); not spending credits.", e);
                    false
                }
            },
        };
        if !applied {
            return finish_waiting_for_quota(&mut ctx, override_seat.as_deref(), &seats);
        }
    }
}

/// Exit 77 because the only way forward is spending credits without consent:
/// print any previous failed attempt and its `Seat:` line, the reason on
/// stderr, and the `Seats:` warning on stdout.
///
/// Re-checked under the lock: 77 is returned only if consent is *still* the
/// sole blocker. If the situation changed while the question was open (quota
/// reset, a grant or `always` set by another process), nothing is spent —
/// the user said wait — and the exit is 75 ("re-run to continue").
fn finish_waiting_for_quota(
    ctx: &mut RunCtx,
    override_seat: Option<&str>,
    seats: &[(String, Option<DateTime<Utc>>)],
) -> Result<i32> {
    let _lock = CodexLock::acquire().context("acquiring codex.lock")?;
    let cfg = SeatConfig::load()?.unwrap_or_default();
    let state = SeatState::load()?;
    let still_needs_consent = matches!(
        pick_now(&cfg, &state, override_seat, &ctx.tried_seats, &ctx.credits_this_run),
        Err(SeatPickError::QuotaUsedCreditsAvailable { .. })
    );
    let code = if still_needs_consent {
        eprintln!(
            "{}.",
            SeatPickError::QuotaUsedCreditsAvailable { seats: seats.to_vec() }
        );
        log_event(
            "quota_exhausted_credits_available",
            &seats.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(","),
            &format!("not spending credits (credits: {})", cfg.rotation.credits),
        );
        EXIT_CREDITS_CONSENT_NEEDED
    } else {
        eprintln!(
            "Not spending credits as asked; the seat situation changed while waiting \
             (quota reset, or consent recorded elsewhere). Nothing was run — re-run to continue."
        );
        EXIT_ALL_SEATS_COOLING
    };
    if let Some(prev) = ctx.last_failure.take() {
        print_attempt(&prev);
        print_failed_seat_line(&cfg, &state, override_seat, &ctx.last_failed_seat);
    }
    print_seat_notice(&cfg, &state, &ctx.credits_this_run);
    Ok(code)
}

/// "Use credits until quota resets": grant each affected workspace until the
/// earliest known reset among its seats. Refused when no reset is known —
/// there is no blanket open-ended grant.
fn grant_until_reset(prompted: &[PromptedSeat]) -> Result<()> {
    let _lock = CodexLock::acquire()?;
    let cfg = SeatConfig::load()?.ok_or_else(|| anyhow::anyhow!("no seats configured"))?;
    let mut state = SeatState::load()?;
    let now = Utc::now();
    // Only seats that still belong to the workspace the user was asked about
    // and are still on credits; their *current* reset time is used.
    let verified: Vec<(String, Option<DateTime<Utc>>)> = prompted
        .iter()
        .filter(|p| cfg.find(&p.seat).is_some() && seat::workspace_key(&cfg, &p.seat) == p.workspace)
        .filter_map(|p| match quota_state(&state.get(&p.seat), now) {
            QuotaState::OnCredits { resets_at } => Some((p.seat.clone(), resets_at)),
            _ => None,
        })
        .collect();
    if verified.is_empty() {
        anyhow::bail!("the seats changed while the question was open; nothing to grant");
    }
    let granted = seat::grant_credits_until_reset(&cfg, &mut state, &verified, now)?;
    state.save()?;
    for (ws_seats, until) in granted {
        log_event(
            "credits_grant",
            &ws_seats.join(","),
            &format!("until {}", until.to_rfc3339()),
        );
    }
    Ok(())
}

fn set_credit_policy(policy: CreditPolicy) -> Result<()> {
    let _lock = CodexLock::acquire()?;
    let mut cfg = SeatConfig::load()?.ok_or_else(|| anyhow::anyhow!("no seats configured"))?;
    cfg.rotation.credits = policy;
    cfg.save()?;
    log_event("credits_mode", "-", &format!("set to {}", policy));
    Ok(())
}

/// One orchestration pass under `CodexLock`: reload config and state, pick,
/// run, classify, rotate. Returns `NeedCreditDecision` (after persisting
/// state and refresh-back) when the only candidates are seats on credits
/// without consent, so the caller can ask with the lock released.
fn orchestrate<F>(
    args: &[String],
    prompt: &str,
    mode: &Mode,
    attempt: &F,
    usage_client: &dyn UsageClient,
    override_seat: Option<&str>,
    ctx: &mut RunCtx,
) -> Result<Step>
where
    F: Fn(&[String], &str, &Mode, bool) -> Result<AttemptResult>,
{
    // Multi-seat path. Lock held for the whole pass — concurrent codex-clean
    // invocations serialise, which avoids the auth.json refresh-write race.
    let _lock = CodexLock::acquire().context("acquiring codex.lock")?;
    let cfg = SeatConfig::load()
        .context("loading seats.toml")?
        .filter(|c| !c.seats.is_empty())
        .ok_or_else(|| anyhow::anyhow!("seats.toml no longer lists any seats"))?;

    // Re-validate config.toml on every multi-seat run. If somebody flipped
    // cli_auth_credentials_store back to "keyring", subsequent codex spawns
    // would silently use the OS keyring instead of our swapped auth.json.
    let store_outcome = seat::ensure_file_credential_store()
        .context("validating ~/.codex/config.toml credential store setting")?;
    if !matches!(store_outcome, seat::FileStoreOutcome::AlreadyFile) {
        eprintln!(
            "Note: re-applied cli_auth_credentials_store = \"file\" to ~/.codex/config.toml."
        );
    }

    let mut state = SeatState::load()?;

    loop {
        // The budget gates the next codex attempt, not the loop: a blocked run
        // must still reach the reset decision after its last failure.
        let budget_left =
            ctx.attempts_used < cfg.rotation.max_retries.saturating_add(1) + ctx.extra_attempts;
        // Before anything touches the slots, stash any token refresh the
        // previously active seat received (from plain `codex`, or from our
        // own last attempt). This must precede any snapshot refresh too: that
        // stages the slot blob into a scratch home, and a stale refresh token
        // there would be rejected as reused. Runs even when the same seat is
        // about to be re-picked. Guarded by identity; never fatal.
        if let Some(prev) = state.active_seat.clone() {
            persist_refresh(&cfg, &prev);
        }

        if !ctx.balanced_refreshed && cfg.rotation.strategy == Strategy::Balanced && override_seat.is_none() {
            ctx.balanced_refreshed = true;
            refresh_stale_usage(&cfg, &mut state, usage_client);
        }

        // A cached reading that still shows a seat exhausted (no credits,
        // reset ahead) keeps it cooling even after its cooldown clock ran out.
        let recooled = usage::reapply_cached_exhaustion(&cfg, &mut state, Utc::now());
        if !recooled.is_empty() {
            state.save()?;
        }

        let this_run = ctx.credits_this_run.clone();
        let this_run = this_run.as_slice();
        let mut pick = pick_now(&cfg, &state, override_seat, &ctx.tried_seats, this_run);

        // Everything cooling (or the pinned seat cooling) for reasons that
        // purchased credits could lift: re-check before giving up.
        if !ctx.blocked_probed && pick_is_blocked(&pick) {
            ctx.blocked_probed = true;
            if probe_blocked_seats(&cfg, &mut state, usage_client, override_seat) {
                pick = pick_now(&cfg, &state, override_seat, &ctx.tried_seats, this_run);
            }
        }

        // A free usage-limit reset beats both waiting and paying, so it is
        // decided here — under the lock, before the credits question — and
        // only for blocks a reset can actually lift.
        //
        // The two policies differ in what they may act on, because they differ
        // in what they do. `auto` redeems instead of paying, so it also covers
        // a run that *would* have proceeded on credits: free beats paid, and an
        // automatic redemption leaves the caller nothing to do. `ask` STOPS the
        // run, so it may only stop a run that could not proceed anyway —
        // blocking a run the user has already consented to pay for leaves them
        // no next step but to redeem, which is not consent.
        let would_pay = pick
            .as_ref()
            .ok()
            .is_some_and(|n| matches!(quota_state(&state.get(n), Utc::now()), QuotaState::OnCredits { .. }));
        let reset_gate = match cfg.rotation.resets {
            ResetPolicy::Never => false,
            ResetPolicy::Auto => pick_is_blocked(&pick) || would_pay,
            ResetPolicy::Ask => pick_is_blocked(&pick),
        };
        if reset_gate {
            match reset_candidates(&cfg, &state, override_seat, Utc::now()) {
                candidates if candidates.is_empty() => {}
                candidates => {
                    if cfg.rotation.resets == ResetPolicy::Auto && ctx.resets_redeemed == 0 {
                        ctx.resets_redeemed += 1;
                        let seat_name = candidates[0].clone();
                        if redeem_reset(&cfg, &mut state, usage_client, &seat_name) {
                            // The seat is usable again: let it be tried once
                            // more even if it already was, and even if the
                            // budget was spent.
                            ctx.tried_seats.retain(|n| *n != seat_name);
                            ctx.extra_attempts += 1;
                            state.save()?;
                            continue;
                        }
                        state.save()?;
                    } else if cfg.rotation.resets == ResetPolicy::Ask {
                        eprintln!(
                            "{} free usage-limit reset(s) available on {} — redeem with `codex-clean seat reset {}`.",
                            reset_count(&state, &candidates[0]),
                            candidates.join(", "),
                            candidates[0]
                        );
                        log_event(
                            "reset_available",
                            &candidates.join(","),
                            "blocked run could be unblocked by a free reset",
                        );
                        if let Some(prev) = ctx.last_failure.take() {
                            print_attempt(&prev);
                            print_failed_seat_line(&cfg, &state, override_seat, &ctx.last_failed_seat);
                        }
                        // Why the run stopped, and every way out of it. Named
                        // only when credits could actually help: pointing at
                        // them when the block is a cooldown is a dead end.
                        // Printed here so the `Seats:` notice stays the last
                        // line — parsers treat that trailing paragraph as
                        // status and everything above it as agent output.
                        let credits_escape = matches!(pick, Err(SeatPickError::QuotaUsedCreditsAvailable { .. }));
                        println!();
                        println!(
                            "Stopped for a free reset (exit {}) — redeem with `codex-clean seat reset {}`{}.",
                            EXIT_RESET_AVAILABLE,
                            candidates[0],
                            if credits_escape {
                                ", or re-run with CODEX_CLEAN_USE_CREDITS=1 to spend credits instead"
                            } else {
                                ""
                            }
                        );
                        print_seat_notice(&cfg, &state, this_run);
                        return Ok(Step::Done(EXIT_RESET_AVAILABLE));
                    }
                }
            }
        }

        let chosen = match pick {
            Ok(name) => name,
            Err(SeatPickError::QuotaUsedCreditsAvailable { seats }) => {
                state.save()?;
                let prompted = seats
                    .into_iter()
                    .map(|(seat_name, resets_at)| PromptedSeat {
                        workspace: seat::workspace_key(&cfg, &seat_name),
                        seat: seat_name,
                        resets_at,
                    })
                    .collect();
                return Ok(Step::NeedCreditDecision(prompted));
            }
            Err(blocked @ SeatPickError::AllSeatsBlocked { .. }) => {
                let code = report_all_blocked(&blocked);
                if let Some(prev) = ctx.last_failure.take() {
                    print_attempt(&prev);
                    print_failed_seat_line(&cfg, &state, override_seat, &ctx.last_failed_seat);
                }
                print_seat_notice(&cfg, &state, this_run);
                return Ok(Step::Done(code));
            }
            Err(e) => {
                if let Some(prev) = ctx.last_failure.take() {
                    eprintln!("{}", e);
                    print_attempt(&prev);
                    print_failed_seat_line(&cfg, &state, override_seat, &ctx.last_failed_seat);
                    print_seat_notice(&cfg, &state, this_run);
                    return Ok(Step::Done(prev.exit_code));
                }
                // A pinned seat that is cooling / needs login: still tell a
                // stdout-only caller why nothing ran.
                print_seat_notice(&cfg, &state, this_run);
                anyhow::bail!("{}", e);
            }
        };

        let now = Utc::now();
        let consent = consent_for(&cfg, &state, &chosen, this_run, now);

        // Best-effort pre-run check: about to run without credit consent on a
        // seat whose last reading is missing, or old and near its limit.
        if consent.is_none()
            && !ctx.prerun_checked.contains(&chosen)
            && needs_prerun_check(&cfg, &state.get(&chosen), now)
        {
            ctx.prerun_checked.push(chosen.clone());
            if let Some(entry) = cfg.find(&chosen).cloned() {
                fetch_and_reconcile(&cfg, &mut state, usage_client, vec![entry], "pre-run check");
            }
            continue; // re-pick with the fresh reading; no attempt consumed
        }

        if !budget_left {
            // Nothing left to spend on another attempt; the terminal block
            // below reports (the reset decision above already had its say).
            break;
        }

        if ctx.tried_seats.contains(&chosen) {
            // We've already tried this seat in this run — guard against loops.
            break;
        }
        ctx.tried_seats.push(chosen.clone());
        ctx.attempts_used += 1;

        let credit_use = match quota_state(&state.get(&chosen), now) {
            QuotaState::OnCredits { .. } => consent,
            _ => None,
        };
        if let Some(cu) = credit_use {
            log_event("on_credits", &chosen, &format!("running on workspace credits ({})", cu.describe()));
        }

        // Eager state update: write last_used before spawning so a future
        // pick (after retry) doesn't reselect the same seat by accident.
        state.entry_mut(&chosen).last_used = Some(now);
        state.save()?;

        swap_active_auth(&chosen)
            .with_context(|| format!("swapping active auth to seat '{}'", chosen))?;
        state.active_seat = Some(chosen.clone());
        state.save()?;

        let attempt_result = attempt(args, prompt, mode, true)?;
        // Codex may have refreshed the OAuth token during the run; persist
        // it into the side store so the next swap doesn't install stale
        // credentials.
        persist_refresh(&cfg, &chosen);

        let kind = classify_attempt(&attempt_result);
        match kind {
            FailureKind::Other if attempt_result.exit_code == 0 && attempt_result.output.errors.is_empty() => {
                let session = attempt_result.output.session_id.clone();
                let entry = state.entry_mut(&chosen);
                entry.consecutive_failures = 0;
                entry.cooldown_until = None;
                entry.cooldown_reason = None;
                entry.last_session = session.clone();
                entry.last_session_at = Some(Utc::now());
                state.save()?;
                print_attempt(&attempt_result);
                // Optional, off by default: one extra app-server round trip.
                let cost = if cfg.rotation.show_session_cost {
                    session
                        .as_deref()
                        .and_then(|id| cfg.find(&chosen).map(|s| (s.clone(), id.to_string())))
                        .and_then(|(entry, id)| {
                            let before = seat::slot_snapshot(&chosen);
                            let got = usage_client.thread_cost(&entry, &id).ok();
                            seat::sync_active_auth(&chosen, before);
                            got
                        })
                        .map(|c| usage::format_cost(&c))
                } else {
                    None
                };
                print_seat_line_with_cost(
                    &cfg,
                    &state,
                    &chosen,
                    override_seat,
                    &ctx.exhausted_so_far,
                    None,
                    credit_use,
                    cost.as_deref(),
                );
                print_seat_notice(&cfg, &state, this_run);
                return Ok(Step::Done(attempt_result.exit_code));
            }
            FailureKind::AuthError => {
                let entry = state.entry_mut(&chosen);
                entry.needs_login = true;
                state.save()?;
                log_event(
                    "auth_error",
                    &chosen,
                    &format!("marked needs_login; {}", failure_excerpt(&attempt_result)),
                );
                eprintln!(
                    "Seat '{}' has invalid credentials. Run: codex-clean seat login {}",
                    chosen, chosen
                );
                print_attempt(&attempt_result);
                print_seat_line(
                    &cfg,
                    &state,
                    &chosen,
                    override_seat,
                    &ctx.exhausted_so_far,
                    Some("auth failed; needs login"),
                    credit_use,
                );
                print_seat_notice(&cfg, &state, this_run);
                return Ok(Step::Done(attempt_result.exit_code));
            }
            FailureKind::RateLimit { recovery, reason } => {
                let cd = ratelimit::apply_recovery_window(
                    recovery,
                    Utc::now(),
                    ratelimit::default_cooldown_for(
                        reason,
                        cfg.rotation.default_cooldown_seconds,
                        cfg.rotation.cooldown_max_seconds,
                    ),
                    cfg.rotation.cooldown_min_seconds,
                    cfg.rotation.cooldown_max_seconds,
                    cfg.rotation.cooldown_jitter_seconds,
                );
                // Personal limits cool this seat only. Credits and spend caps
                // are per *workspace*: every seat sharing this seat's
                // account_id is equally blocked, so cool them together rather
                // than burning an attempt discovering it.
                let affected = affected_seats(&cfg, &chosen, reason);
                cool_seats(&mut state, &affected, cd, reason, Utc::now());
                let entry = state.entry_mut(&chosen);
                entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
                state.save()?;
                let until = state
                    .get(&chosen)
                    .cooldown_until
                    .unwrap_or(cd)
                    .with_timezone(&Local)
                    .format("%a %H:%M")
                    .to_string();
                if affected.len() > 1 {
                    eprintln!(
                        "Seat '{}' exhausted ({}); this applies to the whole workspace, so seats {} are all cooling until {}.",
                        chosen,
                        reason,
                        affected.join(", "),
                        until
                    );
                } else {
                    eprintln!("Seat '{}' exhausted ({}); cooling until {}.", chosen, reason, until);
                }
                log_event(
                    "rate_limit",
                    &chosen,
                    &format!(
                        "reason={} until={} affected={} {}",
                        reason,
                        cd.to_rfc3339(),
                        affected.join(","),
                        failure_excerpt(&attempt_result)
                    ),
                );
                let reason_str = reason.to_string();
                ctx.last_failed_seat = Some(FailedSeat {
                    seat: chosen.clone(),
                    reason: reason_str.clone(),
                    before: ctx.exhausted_so_far.clone(),
                    credit_use,
                });
                ctx.last_failure = Some(attempt_result);
                ctx.exhausted_so_far.push((chosen.clone(), reason_str));
                // A pinned seat does not rotate, but it may still be rescued
                // by a free reset, so loop once more: with the budget spent
                // the next pass only reaches the reset decision.
                continue;
            }
            FailureKind::Other => {
                let _ = log_unmatched(&chosen, &attempt_result);
                print_attempt(&attempt_result);
                print_seat_line(
                    &cfg,
                    &state,
                    &chosen,
                    override_seat,
                    &ctx.exhausted_so_far,
                    Some("run failed"),
                    credit_use,
                );
                print_seat_notice(&cfg, &state, this_run);
                return Ok(Step::Done(attempt_result.exit_code));
            }
        }
    }

    // Rotation ran out of seats to try. If we are not pinned and every
    // configured seat was either rate-limited in this run or is otherwise
    // ineligible right now, that is EX_TEMPFAIL — the same situation the
    // up-front check reports — regardless of how short the cooldowns are.
    // A pinned seat keeps the child's exit code (README contract).
    let this_run = ctx.credits_this_run.clone();
    let this_run = this_run.as_slice();
    match ctx.last_failure.take() {
        Some(prev) => {
            let now = Utc::now();
            let all_blocked = override_seat.is_none()
                && cfg.seats.iter().all(|s| {
                    ctx.tried_seats.contains(&s.name) || !state.get(&s.name).is_eligible(now)
                });
            if all_blocked {
                let code = report_all_blocked(&seat::all_blocked_error(&cfg, &state));
                print_attempt(&prev);
                print_failed_seat_line(&cfg, &state, override_seat, &ctx.last_failed_seat);
                print_seat_notice(&cfg, &state, this_run);
                return Ok(Step::Done(code));
            }
            print_attempt(&prev);
            print_failed_seat_line(&cfg, &state, override_seat, &ctx.last_failed_seat);
            print_seat_notice(&cfg, &state, this_run);
            Ok(Step::Done(prev.exit_code))
        }
        None => Ok(Step::Done(1)),
    }
}

fn pick_now(
    cfg: &SeatConfig,
    state: &SeatState,
    override_seat: Option<&str>,
    tried: &[String],
    this_run: &[String],
) -> Result<String, SeatPickError> {
    let now = Utc::now();
    let credits_for = |s: &str| consent_for(cfg, state, s, this_run, now).is_some();
    seat::pick_seat_excluding(cfg, state, override_seat, now, tried, &credits_for)
}

/// Last used-% at or above which a stale reading is re-checked before a run
/// that has no credit consent.
const PRERUN_CHECK_PERCENT: u32 = 80;

fn needs_prerun_check(cfg: &SeatConfig, st: &seat::SeatRuntimeState, now: DateTime<Utc>) -> bool {
    let probe = chrono::Duration::seconds(cfg.rotation.blocked_probe_seconds as i64);
    if st.usage_checked_at.is_some_and(|t| now - t < probe) {
        return false; // checked (successfully or not) very recently
    }
    match &st.usage {
        None => true,
        Some(u) => now - u.fetched_at > probe && st.usage_score().unwrap_or(0) >= PRERUN_CHECK_PERCENT,
    }
}

/// When every seat (or the pinned seat) is cooling, re-check the seats whose
/// cooldowns purchased credits could lift, at most once per
/// `blocked_probe_seconds` per seat. Returns true when anything was fetched.
fn probe_blocked_seats(
    cfg: &SeatConfig,
    state: &mut SeatState,
    client: &dyn UsageClient,
    override_seat: Option<&str>,
) -> bool {
    let now = Utc::now();
    let probe = chrono::Duration::seconds(cfg.rotation.blocked_probe_seconds as i64);
    let candidates: Vec<seat::SeatEntry> = cfg
        .seats
        .iter()
        .filter(|s| override_seat.is_none_or(|o| o == s.name))
        .filter(|s| {
            let st = state.get(&s.name);
            if st.needs_login {
                return false;
            }
            let cooling = st.cooldown_until.is_some_and(|u| u > now);
            let clearable = ratelimit::CooldownReason::parse(st.cooldown_reason.as_deref().unwrap_or(""))
                .is_clearable_by_credits();
            // Blocked by a cooldown credits could lift, or by needing consent
            // to spend credits — the latter has no cooldown at all, and its
            // reading may predate free-reset support entirely.
            let blocked = (cooling && clearable)
                || matches!(quota_state(&st, now), QuotaState::OnCredits { .. });
            let recently_checked = st.usage_checked_at.is_some_and(|t| now - t < probe);
            let fresh = st.usage.as_ref().is_some_and(|u| now - u.fetched_at < probe);
            blocked && !recently_checked && !fresh
        })
        .cloned()
        .collect();
    if candidates.is_empty() {
        return false;
    }
    fetch_and_reconcile(cfg, state, client, candidates, "all seats blocked");
    true
}

/// For the `balanced` strategy: refresh the usage snapshot of every eligible
/// seat whose snapshot is missing or older than `balance_refresh_seconds`,
/// so the pick reflects real headroom. Never fails the run.
fn refresh_stale_usage(cfg: &SeatConfig, state: &mut SeatState, client: &dyn UsageClient) {
    let now = Utc::now();
    let max_age = chrono::Duration::seconds(cfg.rotation.balance_refresh_seconds as i64);
    let stale: Vec<seat::SeatEntry> = cfg
        .seats
        .iter()
        .filter(|s| {
            let st = state.get(&s.name);
            st.is_eligible(now) && st.usage_is_stale(now, max_age)
        })
        .cloned()
        .collect();
    if stale.is_empty() {
        return;
    }
    fetch_and_reconcile(cfg, state, client, stale, "balanced strategy");
}

/// Fetch usage for `seats`, reconcile the snapshots into state (cooldowns,
/// workspace propagation, credit clearing), mark seats whose tokens are
/// rejected as `needs_login`, save, and keep `~/.codex/auth.json` in step if
/// the active seat's token was rotated during the fetch. Never fails the run.
fn fetch_and_reconcile(
    cfg: &SeatConfig,
    state: &mut SeatState,
    client: &dyn UsageClient,
    seats: Vec<seat::SeatEntry>,
    why: &str,
) {
    let names: Vec<&str> = seats.iter().map(|s| s.name.as_str()).collect();
    eprintln!("Refreshing usage for seat(s) {} ({}).", names.join(", "), why);

    let active = state.active_seat.clone();
    let active_slot_before = active
        .as_deref()
        .and_then(|a| seat::seat_auth_path(a).ok())
        .and_then(|p| std::fs::read(p).ok());

    let now = Utc::now();
    let mut fetched = Vec::new();
    for (name, result) in usage::fetch_all(client, &seats) {
        state.entry_mut(&name).usage_checked_at = Some(now);
        match result {
            Ok(snap) => fetched.push((name, snap)),
            Err(usage::UsageFetchError::AuthRequired) => {
                state.entry_mut(&name).needs_login = true;
                log_event("auth_error", &name, "usage refresh rejected the seat's tokens; marked needs_login");
                eprintln!(
                    "Seat '{}' has invalid credentials (found while refreshing usage). Run: codex-clean seat login {}",
                    name, name
                );
            }
            Err(e) => eprintln!("Warning: could not refresh usage for seat '{}': {}", name, e),
        }
    }
    for (seat_name, notice) in usage::reconcile_snapshots(cfg, state, fetched, now) {
        eprintln!("Note: {}", notice);
        log_event("status", &seat_name, &notice);
    }
    if let Err(e) = state.save() {
        eprintln!("Warning: could not save refreshed usage: {:#}", e);
    }

    if let Some(a) = active.as_deref() {
        let after = seat::seat_auth_path(a).ok().and_then(|p| std::fs::read(p).ok());
        if after.is_some() && after != active_slot_before {
            match swap_active_auth(a) {
                Ok(()) => eprintln!(
                    "Note: seat '{}' refreshed its token during the usage check; ~/.codex/auth.json updated.",
                    a
                ),
                Err(e) => eprintln!(
                    "Warning: could not update ~/.codex/auth.json with seat '{}''s refreshed token: {:#}",
                    a, e
                ),
            }
        }
    }
}

/// Is this pick outcome a block that a free reset or credits could lift?
fn pick_is_blocked(pick: &Result<String, SeatPickError>) -> bool {
    matches!(
        pick,
        Err(SeatPickError::AllSeatsBlocked { .. })
            | Err(SeatPickError::SeatCooling { .. })
            | Err(SeatPickError::QuotaUsedCreditsAvailable { .. })
    )
}

/// How many free resets the seat's last reading reported.
fn reset_count(state: &SeatState, seat_name: &str) -> u32 {
    state
        .get(seat_name)
        .usage
        .and_then(|u| u.resets)
        .map(|r| r.available)
        .unwrap_or(0)
}

/// Seats a free reset could unblock, honouring a pin. The rule itself lives
/// in `seat::reset_eligible_seats`, so the stdout advice and this agree.
fn reset_candidates(
    cfg: &SeatConfig,
    state: &SeatState,
    override_seat: Option<&str>,
    now: DateTime<Utc>,
) -> Vec<String> {
    seat::reset_eligible_seats(cfg, state, now)
        .into_iter()
        .filter(|(n, _, _)| override_seat.is_none_or(|o| o == n))
        .map(|(n, _, _)| n)
        .collect()
}

/// Redeem one free reset for `seat_name`, then re-read its usage so the
/// cooldown can be cleared. Returns true when the windows were actually
/// reset. Never fatal: any failure falls through to the normal 77/75 paths.
fn redeem_reset(
    cfg: &SeatConfig,
    state: &mut SeatState,
    client: &dyn UsageClient,
    seat_name: &str,
) -> bool {
    let Some(entry) = cfg.find(seat_name).cloned() else { return false };
    eprintln!("Redeeming a free usage-limit reset for seat '{}' (credits: resets = auto).", seat_name);
    let before = seat::slot_snapshot(seat_name);
    let outcome = client.consume_reset(&entry, None);
    seat::sync_active_auth(seat_name, before);
    match outcome {
        Ok(ResetOutcome::Reset) => {
            log_event("reset", seat_name, "redeemed a free usage-limit reset (auto)");
            let now = Utc::now();
            state.entry_mut(seat_name).usage_checked_at = Some(now);
            let before = seat::slot_snapshot(seat_name);
            let refreshed = client.fetch(&entry);
            seat::sync_active_auth(seat_name, before);
            let cleared = match refreshed {
                Ok(snap) => {
                    for (seat, notice) in usage::reconcile_snapshots(cfg, state, vec![(seat_name.to_string(), snap)], now) {
                        eprintln!("Note: {}", notice);
                        log_event("status", &seat, &notice);
                    }
                    usage::clear_window_cooldown_after_reset(state, seat_name, Utc::now())
                }
                Err(e) => {
                    // The grant is spent and the windows really were reset, so
                    // the recorded reading is now wrong: drop it and lift the
                    // window cooldown anyway, or the reset would be wasted.
                    eprintln!("Warning: could not re-read usage after the reset: {}", e);
                    usage::invalidate_after_reset(state, seat_name, Utc::now())
                }
            };
            if cleared {
                eprintln!("Seat '{}' is available again after the reset.", seat_name);
            }
            true
        }
        Ok(other) => {
            eprintln!("Free reset not used for seat '{}': {}.", seat_name, other.as_str().replace('_', " "));
            log_event("reset", seat_name, &format!("outcome={}", other.as_str()));
            false
        }
        Err(e) => {
            eprintln!("Warning: could not redeem a free reset for seat '{}': {}", seat_name, e);
            false
        }
    }
}

/// Seats to cool for a failure on `chosen`: just it for a personal limit;
/// every seat in the same workspace for credits / spend caps.
fn affected_seats(cfg: &SeatConfig, chosen: &str, reason: ratelimit::CooldownReason) -> Vec<String> {
    if reason.is_window_based() {
        vec![chosen.to_string()]
    } else {
        workspace_siblings(cfg, chosen)
    }
}

/// Compose the `Seat:` line: which seat ran, under which strategy (or pin),
/// its recorded usage and how old that reading is (and whether it ran on
/// credits, with the consent that applied), which seats were exhausted
/// earlier in this run, and the outcome if the run failed.
///
///   Seat: backup1 (balanced; usage 5h 48% wk 14%, as of 2m ago)
///   Seat: main (balanced; usage 5h 0% wk 100%, on credits (this run), as of 1m ago)
///   Seat: backup1 (balanced; usage unknown; after main exhausted: rate_limit)
#[allow(clippy::too_many_arguments)]
pub fn seat_line(
    seat_name: &str,
    strategy: Strategy,
    pinned: bool,
    usage_snapshot: Option<&seat::UsageSnapshot>,
    exhausted_before: &[(String, String)],
    outcome: Option<&str>,
    credit_use: Option<CreditUse>,
    cost: Option<&str>,
    now: DateTime<Utc>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(if pinned {
        "pinned via CODEX_CLEAN_SEAT".to_string()
    } else {
        strategy.to_string()
    });
    let credits = credit_use
        .map(|c| format!(", on credits ({})", c.describe()))
        .unwrap_or_default();
    match usage_snapshot {
        Some(u) => parts.push(format!(
            "usage {}{}, as of {} ago",
            usage::summarize_usage_short(u),
            credits,
            usage::format_duration_short(now - u.fetched_at)
        )),
        None => parts.push(format!("usage unknown{}", credits)),
    }
    if !exhausted_before.is_empty() {
        let list: Vec<String> = exhausted_before
            .iter()
            .map(|(s, r)| format!("{} exhausted: {}", s, r))
            .collect();
        parts.push(format!("after {}", list.join(", ")));
    }
    if let Some(c) = cost {
        parts.push(format!("cost {}", c));
    }
    if let Some(o) = outcome {
        parts.push(o.to_string());
    }
    format!("Seat: {} ({})", seat_name, parts.join("; "))
}

/// Set to any non-empty value to suppress the `Seat:` line (for logs that
/// are shared or retained where seat names should not appear).
pub const NO_SEAT_LINE_ENV: &str = "CODEX_CLEAN_NO_SEAT_LINE";

fn print_seat_line(
    cfg: &SeatConfig,
    state: &SeatState,
    seat_name: &str,
    override_seat: Option<&str>,
    exhausted_before: &[(String, String)],
    outcome: Option<&str>,
    credit_use: Option<CreditUse>,
) {
    print_seat_line_with_cost(cfg, state, seat_name, override_seat, exhausted_before, outcome, credit_use, None)
}

#[allow(clippy::too_many_arguments)]
fn print_seat_line_with_cost(
    cfg: &SeatConfig,
    state: &SeatState,
    seat_name: &str,
    override_seat: Option<&str>,
    exhausted_before: &[(String, String)],
    outcome: Option<&str>,
    credit_use: Option<CreditUse>,
    cost: Option<&str>,
) {
    if env::var_os(NO_SEAT_LINE_ENV).is_some_and(|v| !v.is_empty()) {
        return;
    }
    let st = state.get(seat_name);
    println!(
        "{}",
        seat_line(
            seat_name,
            cfg.rotation.strategy,
            override_seat.is_some(),
            st.usage.as_ref(),
            exhausted_before,
            outcome,
            credit_use,
            cost,
            Utc::now(),
        )
    );
}

/// `Seat:` line for the attempt held in `last_failure`.
fn print_failed_seat_line(
    cfg: &SeatConfig,
    state: &SeatState,
    override_seat: Option<&str>,
    failed: &Option<FailedSeat>,
) {
    if let Some(f) = failed {
        let outcome = format!("exhausted: {}", f.reason);
        print_seat_line(cfg, state, &f.seat, override_seat, &f.before, Some(&outcome), f.credit_use);
    }
}

/// The degraded-pool summary goes on **stdout**, after the normal output, on
/// every multi-seat run. Background callers read stdout; stderr is discarded.
fn print_seat_notice(cfg: &SeatConfig, state: &SeatState, this_run: &[String]) {
    let now = Utc::now();
    let credits_for = |s: &str| consent_for(cfg, state, s, this_run, now).is_some();
    if let Some(n) = seat_notice(cfg, state, now, &credits_for) {
        println!();
        println!("{}", n);
    }
}

/// Short, single-line description of why an attempt failed, for the events log.
fn failure_excerpt(attempt: &AttemptResult) -> String {
    let text = if !attempt.output.errors.is_empty() {
        attempt.output.errors.join(" | ")
    } else if let Some(m) = attempt.output.messages.last() {
        m.clone()
    } else {
        String::from_utf8_lossy(&attempt.stderr_buffer)
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .to_string()
    };
    let text: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let mut out: String = text.trim().chars().take(300).collect();
    if text.trim().chars().count() > 300 {
        out.push('…');
    }
    format!("exit={} msg={:?}", attempt.exit_code, out)
}

/// Persist the active `~/.codex/auth.json` into `seat`'s slot if it belongs
/// to that seat. Warns (never fails) on skip or error.
fn persist_refresh(cfg: &SeatConfig, seat_name: &str) {
    let expected = cfg.identity_for(seat_name);
    match refresh_back_guarded(seat_name, &expected) {
        Ok(outcome) => warn_refresh_back(seat_name, &outcome),
        Err(e) => eprintln!(
            "Warning: failed to persist refreshed token for seat '{}' to its side store: {:#}. \
             If subsequent runs fail with auth errors, run `codex-clean seat login {}`.",
            seat_name, e, seat_name
        ),
    }
}

/// Print the "nothing eligible" message and return the exit code: 75 when at
/// least one seat is merely cooling (retry later), 1 when every seat needs a
/// login (user action required, so a retry loop would spin for nothing).
fn report_all_blocked(err: &SeatPickError) -> i32 {
    eprintln!("{}.", err);
    match err {
        SeatPickError::AllSeatsBlocked { cooling, .. } if *cooling > 0 => EXIT_ALL_SEATS_COOLING,
        _ => 1,
    }
}

/// Classify an attempt's outcome, falling back to stderr text when the
/// structured `output.errors` list is empty (e.g. codex died before
/// emitting any JSON events).
fn classify_attempt(attempt: &AttemptResult) -> FailureKind {
    if !attempt.output.errors.is_empty() {
        let kind = ratelimit::classify(&attempt.output.errors);
        if !matches!(kind, FailureKind::Other) {
            return kind;
        }
    }
    if !attempt.status_success || attempt.exit_code != 0 {
        // Codex sometimes surfaces exhaustion as the *final agent message*
        // rather than an error event ("Your workspace is out of credits. Add
        // credits to continue." arrives as task_complete). Only the LAST
        // message is consulted, only its opening (provider notices are one
        // short sentence, whereas agent prose that merely mentions credits is
        // long), and only when the run failed.
        let mut blob = String::from_utf8_lossy(&attempt.stderr_buffer).into_owned();
        if let Some(last) = attempt.output.messages.last() {
            blob.push('\n');
            blob.extend(last.chars().take(FINAL_MESSAGE_CLASSIFY_CHARS));
        }
        return ratelimit::classify_text(&blob);
    }
    FailureKind::Other
}

/// How much of a failed run's final agent message is examined for an
/// exhaustion notice. Codex's own notices lead with the sentence.
const FINAL_MESSAGE_CLASSIFY_CHARS: usize = 200;

/// Record an unclassified failure for offline pattern tuning. The log is
/// 0600 (via `append_private_log`): the captured text can include model
/// output and error payloads, so it is treated like a credential file. The
/// parsed error events and the final agent message are included — they are
/// where codex puts the sentence we failed to match, and stderr alone has
/// proven useless for diagnosis.
fn log_unmatched(seat: &str, attempt: &AttemptResult) -> Result<()> {
    let path = unmatched_log_path()?;
    let stderr = String::from_utf8_lossy(&attempt.stderr_buffer);
    let tail: Vec<&str> = stderr.lines().rev().take(20).collect();
    let tail = log_excerpt(&tail.into_iter().rev().collect::<Vec<_>>().join("\n"), 4000);
    let errors = if attempt.output.errors.is_empty() {
        "(none)".to_string()
    } else {
        log_excerpt(&attempt.output.errors.join("\n  "), 2000)
    };
    let last_message = attempt
        .output
        .messages
        .last()
        .map(|m| log_excerpt(m, 500))
        .unwrap_or_else(|| "(none)".to_string());
    let entry = format!(
        "{} seat={} exit={} errors<<<\n  {}\n>>> last_message<<<\n{}\n>>> stderr_tail<<<\n{}\n>>>\n",
        Utc::now().to_rfc3339(),
        seat,
        attempt.exit_code,
        errors,
        last_message,
        tail
    );
    seat::append_private_log(&path, &entry)
}

/// Print captured stderr (when failure) and the formatted output. Mirrors
/// the pre-seat printing behaviour exactly.
pub fn print_attempt(attempt: &AttemptResult) {
    if !attempt.status_success {
        if !attempt.stderr_buffer.is_empty() {
            eprintln!("--- codex stderr ---");
            let _ = io::stderr().write_all(&attempt.stderr_buffer);
            if attempt.stderr_truncated {
                eprintln!("(stderr truncated to {} bytes)", STDERR_CAP_BYTES);
            }
            if let Some(err) = &attempt.stderr_error {
                eprintln!("(failed to capture full stderr: {})", err);
            }
            eprintln!("--- end stderr ---");
        } else if let Some(err) = &attempt.stderr_error {
            eprintln!("--- codex stderr ---");
            eprintln!("Failed to capture stderr: {}", err);
            eprintln!("--- end stderr ---");
        }

        if attempt.output.lines_seen == 0 {
            eprintln!("Codex exited with code {} and produced no JSON output", attempt.child_exit);
        } else if attempt.output.events_recognized == 0 {
            eprintln!(
                "Codex exited with code {} and produced no recognized JSON events",
                attempt.child_exit
            );
        }
    } else if let Some(err) = &attempt.stderr_error {
        eprintln!("Warning: Failed to capture codex stderr: {}", err);
    }

    attempt.output.print();
}

/// One codex spawn-and-collect cycle. Captures stdout/stderr but does not
/// print them; callers decide whether this attempt is the "final" one to
/// surface to the user.
pub fn attempt_codex(
    args: &[String],
    prompt: &str,
    mode: &Mode,
    scrub_env: bool,
) -> Result<AttemptResult> {
    let mut cmd = Command::new("codex");

    // All modes use "codex exec" with --json for JSON output
    cmd.arg("exec");

    if scrub_env {
        for var in SCRUB_ENV_VARS {
            cmd.env_remove(var);
        }
    }
    // Monetary consent is for this codex-clean invocation only; it must never
    // reach codex or anything codex runs, even on the no-seats passthrough.
    for var in seat::CONSENT_ENV_VARS {
        cmd.env_remove(var);
    }

    let mut use_stdin_for_prompt = false;

    match mode {
        Mode::Exec => {
            cmd.arg("--json");
            cmd.arg("--skip-git-repo-check");
            cmd.args(args);
            cmd.arg(prompt);
        }
        Mode::Resume(target) => {
            cmd.arg("--json");
            cmd.arg("--skip-git-repo-check");
            cmd.arg("resume");
            match target {
                ResumeTarget::SessionId(id) => {
                    cmd.arg(id);
                    if !prompt.is_empty() {
                        cmd.arg(prompt);
                    }
                }
                ResumeTarget::Last => {
                    cmd.arg("--last");
                    if !prompt.is_empty() {
                        use_stdin_for_prompt = true;
                    }
                }
            }
        }
        Mode::Review => {
            cmd.arg("review");
            cmd.arg("--json");
            cmd.arg("--skip-git-repo-check");
            cmd.args(args);
            if !prompt.is_empty() {
                cmd.arg(prompt);
            }
        }
    }

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    if use_stdin_for_prompt {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }

    let mut child = cmd.spawn().context("Failed to spawn codex process")?;

    if use_stdin_for_prompt {
        if let Some(mut stdin) = child.stdin.take() {
            writeln!(stdin, "{}", prompt)?;
            stdin.flush()?;
        }
    }

    let stderr = child.stderr.take().expect("stderr was piped");
    let stderr_handle = thread::spawn(move || capture_stderr(stderr));

    let stdout = child.stdout.take().expect("stdout was piped");
    let reader = BufReader::new(stdout);
    let parse_result = parse_codex_stream(reader);

    if parse_result.is_err() {
        let _ = child.kill();
    }

    let status: ExitStatus = child.wait().context("Failed to wait for codex process")?;
    let (stderr_buffer, stderr_truncated, stderr_error) =
        stderr_handle.join().expect("stderr thread panicked");
    let output = parse_result.context("Failed to read codex stdout")?;

    let child_exit = status.code().unwrap_or(1);
    let exit_code = if child_exit == 0 && !output.errors.is_empty() {
        1
    } else {
        child_exit
    };

    Ok(AttemptResult {
        output,
        stderr_buffer,
        stderr_truncated,
        stderr_error,
        exit_code,
        status_success: status.success(),
        child_exit,
    })
}

pub fn parse_codex_stream<R: BufRead>(reader: R) -> io::Result<CodexOutput> {
    let mut output = CodexOutput::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        output.lines_seen += 1;

        if let Some(event) = extract_event(&line) {
            output.events_recognized += 1;
            match event {
                Event::ThreadStarted { thread_id } => {
                    output.add_thread_id(thread_id);
                }
                Event::AgentMessage { text } => {
                    if let Some(t) = text {
                        output.add_message(t);
                    }
                }
                Event::TurnCompleted {
                    input_tokens,
                    cached_input_tokens,
                    output_tokens,
                    reasoning_output_tokens,
                } => {
                    output.add_usage(
                        input_tokens,
                        cached_input_tokens,
                        output_tokens,
                        reasoning_output_tokens,
                    );
                }
                Event::TurnFailed { message } | Event::StreamError { message } => {
                    output.add_error(message);
                }
            }
        }
    }

    Ok(output)
}

fn capture_stderr(stderr: impl Read) -> (Vec<u8>, bool, Option<io::Error>) {
    let mut reader = BufReader::new(stderr);
    let mut buffer = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 4096];

    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let remaining = STDERR_CAP_BYTES.saturating_sub(buffer.len());
                if remaining == 0 {
                    truncated = true;
                    continue;
                }

                let to_copy = remaining.min(n);
                buffer.extend_from_slice(&chunk[..to_copy]);
                if to_copy < n {
                    truncated = true;
                }
            }
            Err(e) => return (buffer, truncated, Some(e)),
        }
    }

    (buffer, truncated, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn seat_line_formats_each_case() {
        let now = Utc::now();
        let snap = seat::UsageSnapshot {
            fetched_at: now - chrono::Duration::minutes(2),
            plan_type: Some("team".into()),
            buckets: vec![seat::UsageBucket {
                limit_id: Some("codex".into()),
                limit_name: None,
                windows: vec![
                    seat::UsageWindow { window_minutes: Some(300), used_percent: 48, resets_at: None },
                    seat::UsageWindow { window_minutes: Some(10080), used_percent: 14, resets_at: None },
                ],
                rate_limit_reached_type: None,
            }],
            credits: None,
            spend_control_reached: None,
            resets: None,
        };
        assert_eq!(
            seat_line("backup1", Strategy::Balanced, false, Some(&snap), &[], None, None, None, now),
            "Seat: backup1 (balanced; usage 5h 48% wk 14%, as of 2m ago)"
        );
        let before = vec![("main".to_string(), "rate_limit".to_string())];
        assert_eq!(
            seat_line("backup1", Strategy::LeastRecentlyUsed, false, None, &before, None, None, None, now),
            "Seat: backup1 (least-recently-used; usage unknown; after main exhausted: rate_limit)"
        );
        assert_eq!(
            seat_line("main", Strategy::Balanced, true, Some(&snap), &[], Some("exhausted: credits"), None, None, now),
            "Seat: main (pinned via CODEX_CLEAN_SEAT; usage 5h 48% wk 14%, as of 2m ago; exhausted: credits)"
        );
    }

    #[test]
    fn parse_codex_stream_extracts_events() {
        let data = r#"
{"type":"thread.started","thread_id":"session-1"}
{"type":"item.completed","item":{"type":"agent_message","text":"hello"}}
{"type":"item.completed","item":{"type":"agent_message","text":"world"}}
"#;
        let cursor = Cursor::new(data);
        let output = parse_codex_stream(BufReader::new(cursor)).unwrap();
        assert_eq!(output.session_id, Some("session-1".to_string()));
        assert_eq!(output.messages, vec!["hello".to_string(), "world".to_string()]);
    }

    #[test]
    fn parse_codex_stream_extracts_usage() {
        let data = r#"
{"type":"thread.started","thread_id":"session-1"}
{"type":"item.completed","item":{"type":"agent_message","text":"hello"}}
{"type":"turn.completed","usage":{"input_tokens":15228,"cached_input_tokens":14208,"output_tokens":249,"reasoning_output_tokens":64}}
"#;
        let cursor = Cursor::new(data);
        let output = parse_codex_stream(BufReader::new(cursor)).unwrap();
        assert_eq!(output.session_id, Some("session-1".to_string()));
        assert_eq!(output.messages, vec!["hello".to_string()]);
        assert_eq!(output.usage, Some((15228, 14208, 249, 64)));
    }

    #[test]
    fn parse_codex_stream_captures_turn_failed() {
        let data = r#"
{"type":"thread.started","thread_id":"session-err"}
{"type":"turn.started"}
{"type":"turn.failed","error":{"message":"invalid_request_error: bad effort"}}
"#;
        let cursor = Cursor::new(data);
        let output = parse_codex_stream(BufReader::new(cursor)).unwrap();
        assert_eq!(output.session_id, Some("session-err".to_string()));
        assert_eq!(output.errors.len(), 1);
        assert!(output.errors[0].contains("invalid_request_error"));
    }

    #[test]
    fn parse_codex_stream_captures_stream_error() {
        let data = r#"
{"type":"thread.started","thread_id":"session-err"}
{"type":"error","message":"connection reset"}
"#;
        let cursor = Cursor::new(data);
        let output = parse_codex_stream(BufReader::new(cursor)).unwrap();
        assert_eq!(output.errors, vec!["connection reset".to_string()]);
    }

    #[test]
    fn parse_codex_stream_tracks_line_counts() {
        let data = r#"
{"type":"thread.started","thread_id":"s1"}
{"type":"unknown.thing","data":"ignored"}
{"type":"item.completed","item":{"type":"agent_message","text":"hi"}}
not json at all
"#;
        let cursor = Cursor::new(data);
        let output = parse_codex_stream(BufReader::new(cursor)).unwrap();
        assert_eq!(output.lines_seen, 4);
        assert_eq!(output.events_recognized, 2); // thread.started + agent_message
    }

    #[test]
    fn parse_codex_stream_all_unrecognized() {
        let data = r#"
{"type":"new.unknown","data":"x"}
{"type":"another.unknown","data":"y"}
"#;
        let cursor = Cursor::new(data);
        let output = parse_codex_stream(BufReader::new(cursor)).unwrap();
        assert_eq!(output.lines_seen, 2);
        assert_eq!(output.events_recognized, 0);
        let rendered = output.render();
        assert!(rendered.stderr.contains("none matched known event types"));
    }

    #[test]
    fn parse_codex_stream_propagates_errors() {
        // Invalid UTF-8 sequence should trigger an error from lines()
        let data = b"\x80\x80";
        let cursor = Cursor::new(&data[..]);
        let err = parse_codex_stream(BufReader::new(cursor)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
