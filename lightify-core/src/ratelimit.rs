//! Process-wide Spotify Web-API rate gate.
//!
//! Before this existed, a 429 was only ever noticed by whichever caller happened to
//! receive it, and only the shell's main poll loop kept a backoff deadline. Every
//! other requester — the background sidebar fetch, the Beatport refill task, a bulk
//! queue loop, the post-command `settle_refresh` — has its own `Session` and kept
//! firing straight into the limit, which is exactly what prolongs it (Spotify's
//! window is a rolling 30 s, so each rejected request keeps the window full).
//!
//! Now every Web-API request goes through [`admit`] first. While a cool-down is
//! active it fails fast, *without touching the network*, using the very same
//! `"Rate limited by Spotify — retry in {n}s"` text a real 429 produces — so every
//! existing caller that already backs off on that string (`rate_limit_backoff` in the
//! shell) handles a locally-blocked request identically, with the remaining time.
//!
//! On top of the hard block this keeps a rolling 30 s count of requests actually
//! sent ([`recent_requests`]) so *background* traffic can yield when the window is
//! busy, leaving the budget for what the user is doing (see [`background_ok`]).

use std::collections::VecDeque;
use std::hash::{BuildHasher, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Spotify computes its limit over a rolling 30-second window.
pub const WINDOW: Duration = Duration::from_secs(30);

/// Background polls stand down once this many requests went out in the last
/// [`WINDOW`]. Dev-mode quotas are low and shared with the shipped app on the same
/// client id, so this is deliberately conservative; interactive calls ignore it.
pub const BACKGROUND_BUDGET: usize = 45;

/// Upper bound on any single cool-down (a QUOTA_EXCEEDED `Retry-After` can be long,
/// but an hour is the most we will ever sit silent).
const MAX_BLOCK_SECS: u64 = 3600;

struct State {
    blocked_until: Option<Instant>,
    /// Consecutive 429s with no success in between — drives the fallback
    /// exponential backoff when Spotify omits `Retry-After`.
    strikes: u32,
    /// Send times inside the last `WINDOW`, oldest first.
    sent: VecDeque<Instant>,
}

static STATE: Mutex<State> =
    Mutex::new(State { blocked_until: None, strikes: 0, sent: VecDeque::new() });

fn state() -> std::sync::MutexGuard<'static, State> {
    // A panic while holding this lock can't leave it logically inconsistent (every
    // field is independently valid), so recover from poisoning rather than cascade.
    STATE.lock().unwrap_or_else(|p| p.into_inner())
}

fn prune(s: &mut State, now: Instant) {
    while s.sent.front().is_some_and(|t| now.duration_since(*t) > WINDOW) {
        s.sent.pop_front();
    }
}

/// The standard rate-limit error text. Kept in one place so the real-429 path and
/// the local fast-fail path can never drift apart (the shell parses it).
pub fn rate_limited_error(secs: u64) -> String {
    format!("Rate limited by Spotify \u{2014} retry in {secs}s")
}

/// Is `e` a rate-limit error (real or locally gated)?
pub fn is_rate_limited(e: &str) -> bool {
    e.starts_with("Rate limited")
}

/// How much longer the cool-down lasts, if one is active.
pub fn blocked_for() -> Option<Duration> {
    let now = Instant::now();
    let s = state();
    s.blocked_until.filter(|t| *t > now).map(|t| t - now)
}

/// Gate one outgoing Web-API request: `Err` (no network) during a cool-down,
/// otherwise the request is counted against the rolling window.
pub(crate) fn admit() -> Result<(), String> {
    let now = Instant::now();
    let mut s = state();
    if let Some(until) = s.blocked_until {
        if until > now {
            let secs = (until - now).as_secs() + 1;
            return Err(rate_limited_error(secs));
        }
        s.blocked_until = None;
    }
    prune(&mut s, now);
    s.sent.push_back(now);
    Ok(())
}

/// A request came back without a 429 — the penalty streak is over.
pub(crate) fn note_ok() {
    let mut s = state();
    s.strikes = 0;
}

/// Record a 429 and start (or extend) the process-wide cool-down. Returns the
/// number of seconds actually applied.
///
/// * Honours `Retry-After` when present; otherwise backs off exponentially per
///   consecutive strike (5, 10, 20 … capped at 5 min) instead of a flat 5 s that a
///   sustained limit would just keep tripping.
/// * Adds 0–20 % jitter. The shipped app shares this client id and receives the
///   *same* `Retry-After`; without jitter both resume on the same instant and
///   immediately refill the window together.
/// * Never shortens an existing block (two in-flight requests can both 429 with
///   different values; the longer one wins).
pub(crate) fn note_429(retry_after: Option<u64>) -> u64 {
    let mut s = state();
    s.strikes = s.strikes.saturating_add(1);
    let base = retry_after
        .unwrap_or_else(|| 5u64.saturating_mul(1u64 << (s.strikes - 1).min(6)).min(300))
        .clamp(1, MAX_BLOCK_SECS);
    let secs = (base + jitter(base)).min(MAX_BLOCK_SECS);
    let until = Instant::now() + Duration::from_secs(secs);
    if s.blocked_until.map_or(true, |t| until > t) {
        s.blocked_until = Some(until);
    }
    secs
}

/// Requests sent in the last [`WINDOW`].
pub fn recent_requests() -> usize {
    let mut s = state();
    prune(&mut s, Instant::now());
    s.sent.len()
}

/// Should an optional background request (a poll, a prefetch) go out right now?
/// False during a cool-down or when the rolling window is already busy — the
/// caller should simply try again on its next tick.
pub fn background_ok() -> bool {
    blocked_for().is_none() && recent_requests() < BACKGROUND_BUDGET
}

/// 0–20 % of `base`, from the std hasher's per-process random seed (no `rand` dep).
fn jitter(base: u64) -> u64 {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(base);
    let r = h.finish();
    let span = base / 5;
    if span == 0 { 0 } else { r % (span + 1) }
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test touches the global state so ordering between tests can't matter.
    #[test]
    fn gate_blocks_then_counts() {
        assert!(is_rate_limited(&rate_limited_error(3)));
        let applied = note_429(Some(2));
        assert_eq!(applied, 2, "jitter is 0 below 5s");
        let e = admit().unwrap_err();
        assert!(is_rate_limited(&e), "{e}");
        assert!(blocked_for().is_some());
        assert!(!background_ok());
        // Clear the block the way expiry would, then make sure sends are counted.
        state().blocked_until = None;
        note_ok();
        let before = recent_requests();
        admit().unwrap();
        assert_eq!(recent_requests(), before + 1);
        // Missing Retry-After backs off exponentially.
        let a = note_429(None);
        let b = note_429(None);
        assert!(b >= a, "{a} then {b}");
        state().blocked_until = None;
        note_ok();
    }
}
