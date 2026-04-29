//! Rate-limit and auth-error classification of codex output.
//!
//! Pure parsing — no I/O. The strings we anchor on are sourced from
//! `codex-rs/protocol/src/error.rs` and `codex-rs/login/src/auth/manager.rs`
//! (codex 0.125.0). They're hardcoded English upstream; no i18n.

use chrono::{DateTime, Duration, Local, NaiveDateTime, NaiveTime, TimeZone, Utc};

/// Classification of a codex run's failure mode, derived from the messages
/// surfaced via `turn.failed` / `error` events (and stderr fallback).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureKind {
    /// Account or per-model usage cap reached. `recovery` is the wall-clock
    /// time codex reported, if any (already in UTC).
    RateLimit { recovery: Option<DateTime<Utc>> },
    /// Refresh token expired/reused — seat needs re-login, not a cooldown.
    AuthError,
    /// Some other failure we don't classify; caller should bubble exit code
    /// up unchanged and log the case for offline pattern tuning.
    Other,
}

/// Classify a list of error messages from a codex run.
pub fn classify(messages: &[String]) -> FailureKind {
    let blob = messages.join("\n");
    classify_text(&blob)
}

/// Classify a free-form text blob (e.g. captured stderr) for the same patterns.
pub fn classify_text(blob: &str) -> FailureKind {
    if is_auth_error(blob) {
        return FailureKind::AuthError;
    }
    if is_rate_limit(blob) {
        let recovery = parse_recovery_time(blob, Local::now()).map(|d| d.with_timezone(&Utc));
        return FailureKind::RateLimit { recovery };
    }
    FailureKind::Other
}

fn is_rate_limit(s: &str) -> bool {
    s.contains("You've hit your usage limit")
}

fn is_auth_error(s: &str) -> bool {
    s.contains("refresh token has expired")
        || s.contains("refresh_token_expired")
        || s.contains("refresh_token_reused")
}

/// Find a `(try) again at <time>` clause and resolve it to a local timestamp.
/// Returns None if no anchor is present or parsing fails.
pub fn parse_recovery_time(s: &str, now: DateTime<Local>) -> Option<DateTime<Local>> {
    let anchor = "again at ";
    let lower = s.to_lowercase();
    let idx = lower.find(anchor)?;
    let after = &s[idx + anchor.len()..];
    let frag = after.split('.').next()?.trim();

    if let Some(t) = parse_same_day_time(frag, now) {
        return Some(t);
    }
    parse_different_day_time(frag)
}

fn parse_same_day_time(frag: &str, now: DateTime<Local>) -> Option<DateTime<Local>> {
    let t = NaiveTime::parse_from_str(frag, "%I:%M %p").ok()?;
    let date = now.date_naive();
    // .earliest() preserves the earlier of two mappings during a DST fall-back
    // (rather than failing as .single() would), and returns None for
    // nonexistent spring-forward wall times — letting the caller fall back to
    // the configured default cooldown.
    if let Some(candidate) = resolve_local(NaiveDateTime::new(date, t)) {
        if candidate > now {
            return Some(candidate);
        }
    }
    // Past — advance by one calendar day, preserving the local wall-clock
    // time. (Plain `+ Duration::days(1)` would slide an hour on DST days.)
    let tomorrow = date.succ_opt()?;
    resolve_local(NaiveDateTime::new(tomorrow, t))
}

fn parse_different_day_time(frag: &str) -> Option<DateTime<Local>> {
    let cleaned = strip_ordinal_suffix(frag);
    let dt = NaiveDateTime::parse_from_str(&cleaned, "%b %d, %Y %I:%M %p").ok()?;
    resolve_local(dt)
}

/// Resolve a naive local datetime to a concrete `DateTime<Local>`, picking
/// the earlier of two mappings when a fall-back DST transition makes the
/// wall-clock time ambiguous, and returning `None` for nonexistent times in
/// a spring-forward window.
fn resolve_local(dt: NaiveDateTime) -> Option<DateTime<Local>> {
    Local.from_local_datetime(&dt).earliest()
}

fn strip_ordinal_suffix(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        out.push(bytes[i] as char);
        if (bytes[i] as char).is_ascii_digit() && i + 2 < bytes.len() {
            let suffix = &s[i + 1..i + 3];
            if matches!(suffix, "st" | "nd" | "rd" | "th") {
                i += 3;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Compute the cooldown timestamp from a parsed recovery time.
/// Adds jitter, clamps total cooldown duration to `[min, max]`, and falls
/// back to `default_seconds` when no time was parsed.
pub fn apply_recovery_window(
    parsed: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    default_seconds: u64,
    min_seconds: u64,
    max_seconds: u64,
    jitter_seconds: u64,
) -> DateTime<Utc> {
    let target = match parsed {
        Some(t) => t + Duration::seconds(jitter_seconds as i64),
        None => now + Duration::seconds(default_seconds as i64),
    };
    let elapsed = (target - now).num_seconds().max(0) as u64;
    let clamped = elapsed.clamp(min_seconds, max_seconds);
    now + Duration::seconds(clamped as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn local(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, m, d, h, min, 0).single().unwrap()
    }

    #[test]
    fn rate_limit_detected_admin_variant() {
        let msg = "You've hit your usage limit. To get more access now, send a request to your admin or try again at 5:32 PM.";
        match classify_text(msg) {
            FailureKind::RateLimit { recovery } => assert!(recovery.is_some()),
            other => panic!("expected RateLimit, got {:?}", other),
        }
    }

    #[test]
    fn rate_limit_detected_plus_variant() {
        let msg = "You've hit your usage limit. Upgrade to Pro to continue using Codex. Try again at 5:32 PM.";
        assert!(matches!(classify_text(msg), FailureKind::RateLimit { .. }));
    }

    #[test]
    fn rate_limit_no_recovery_time_variant() {
        let msg = "You've hit your usage limit. Try again later.";
        match classify_text(msg) {
            FailureKind::RateLimit { recovery } => assert!(recovery.is_none()),
            other => panic!("expected RateLimit, got {:?}", other),
        }
    }

    #[test]
    fn auth_error_detected_human_message() {
        let msg = "Your access token could not be refreshed because your refresh token has expired. Please log out and sign in again.";
        assert_eq!(classify_text(msg), FailureKind::AuthError);
    }

    #[test]
    fn auth_error_detected_code_string() {
        assert_eq!(classify_text("refresh_token_expired"), FailureKind::AuthError);
        assert_eq!(classify_text("refresh_token_reused"), FailureKind::AuthError);
    }

    #[test]
    fn auth_error_takes_precedence_over_rate_limit() {
        // If both classes match (unlikely in practice), auth wins because
        // a stale refresh token won't recover from a cooldown window.
        let msg = "You've hit your usage limit. Try again at 5:32 PM. refresh_token_expired";
        assert_eq!(classify_text(msg), FailureKind::AuthError);
    }

    #[test]
    fn other_failures_classified() {
        assert_eq!(classify_text("invalid_request_error: bad reasoning effort"), FailureKind::Other);
        assert_eq!(classify_text("connection reset"), FailureKind::Other);
        assert_eq!(classify_text(""), FailureKind::Other);
    }

    #[test]
    fn parse_same_day_future_time() {
        let now = local(2026, 4, 28, 13, 48);
        let parsed = parse_recovery_time("try again at 5:32 PM.", now).unwrap();
        assert_eq!(parsed, local(2026, 4, 28, 17, 32));
    }

    #[test]
    fn parse_same_day_past_time_advances_to_tomorrow() {
        let now = local(2026, 4, 28, 18, 0);
        let parsed = parse_recovery_time("try again at 5:32 PM.", now).unwrap();
        assert_eq!(parsed, local(2026, 4, 29, 17, 32));
    }

    #[test]
    fn parse_different_day_with_ordinal() {
        let now = local(2026, 4, 28, 13, 48);
        let parsed = parse_recovery_time("Try again at Apr 30th, 2026 5:32 PM.", now).unwrap();
        assert_eq!(parsed, local(2026, 4, 30, 17, 32));
    }

    #[test]
    fn parse_different_day_first_of_month_ordinal_st() {
        let now = local(2026, 4, 28, 13, 48);
        let parsed = parse_recovery_time("Try again at May 1st, 2026 5:32 PM.", now).unwrap();
        assert_eq!(parsed, local(2026, 5, 1, 17, 32));
    }

    #[test]
    fn parse_recovery_time_no_anchor_returns_none() {
        let now = local(2026, 4, 28, 13, 48);
        assert!(parse_recovery_time("You've hit your usage limit. Try again later.", now).is_none());
    }

    #[test]
    fn parse_recovery_time_unparseable_fragment_returns_none() {
        let now = local(2026, 4, 28, 13, 48);
        assert!(parse_recovery_time("again at maybe-tomorrow.", now).is_none());
    }

    #[test]
    fn parse_recovery_time_handles_url_periods_correctly() {
        // Plus-variant message has a URL with periods BEFORE the recovery time.
        // Anchor is "again at"; the post-anchor split('.') only sees ".\nfoo" etc.
        let now = local(2026, 4, 28, 13, 48);
        let msg = "You've hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits, or try again at 5:32 PM.";
        let parsed = parse_recovery_time(msg, now).unwrap();
        assert_eq!(parsed, local(2026, 4, 28, 17, 32));
    }

    #[test]
    fn apply_recovery_window_clamps_below_min() {
        let now = Utc.with_ymd_and_hms(2026, 4, 28, 12, 0, 0).unwrap();
        // Parsed time 1 minute ahead → with jitter 120s = 180s. Clamped up to 300s.
        let parsed = Some(now + Duration::seconds(60));
        let result = apply_recovery_window(parsed, now, 3600, 300, 86400, 120);
        assert_eq!(result, now + Duration::seconds(300));
    }

    #[test]
    fn apply_recovery_window_clamps_above_max() {
        let now = Utc.with_ymd_and_hms(2026, 4, 28, 12, 0, 0).unwrap();
        // Parsed 48h ahead → clamped to 24h.
        let parsed = Some(now + Duration::hours(48));
        let result = apply_recovery_window(parsed, now, 3600, 300, 86400, 120);
        assert_eq!(result, now + Duration::seconds(86400));
    }

    #[test]
    fn apply_recovery_window_in_range_keeps_jitter() {
        let now = Utc.with_ymd_and_hms(2026, 4, 28, 12, 0, 0).unwrap();
        let parsed = Some(now + Duration::hours(2));
        let result = apply_recovery_window(parsed, now, 3600, 300, 86400, 120);
        // 2h + 120s jitter = 7320s, well within bounds.
        assert_eq!(result, now + Duration::seconds(2 * 3600 + 120));
    }

    #[test]
    fn apply_recovery_window_falls_back_to_default_when_unparsed() {
        let now = Utc.with_ymd_and_hms(2026, 4, 28, 12, 0, 0).unwrap();
        let result = apply_recovery_window(None, now, 3600, 300, 86400, 120);
        assert_eq!(result, now + Duration::seconds(3600));
    }

    #[test]
    fn apply_recovery_window_negative_target_clamps_to_min() {
        let now = Utc.with_ymd_and_hms(2026, 4, 28, 12, 0, 0).unwrap();
        // Parsed time in the past (clock skew worst-case) → elapsed=0 → clamps up to min.
        let parsed = Some(now - Duration::hours(1));
        let result = apply_recovery_window(parsed, now, 3600, 300, 86400, 120);
        assert_eq!(result, now + Duration::seconds(300));
    }

    #[test]
    fn strip_ordinal_suffix_handles_all_suffixes() {
        assert_eq!(strip_ordinal_suffix("Apr 1st, 2026"), "Apr 1, 2026");
        assert_eq!(strip_ordinal_suffix("Apr 2nd, 2026"), "Apr 2, 2026");
        assert_eq!(strip_ordinal_suffix("Apr 3rd, 2026"), "Apr 3, 2026");
        assert_eq!(strip_ordinal_suffix("Apr 28th, 2026"), "Apr 28, 2026");
    }

    #[test]
    fn strip_ordinal_suffix_no_suffix_no_change() {
        assert_eq!(strip_ordinal_suffix("Apr 28, 2026"), "Apr 28, 2026");
        assert_eq!(strip_ordinal_suffix("5:32 PM"), "5:32 PM");
    }
}
