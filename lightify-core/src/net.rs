//! Process-wide connectivity state (UI-PLAN D8).
//!
//! Every Web-API request goes through `spotify::send_api`, which reports here: a
//! request that couldn't reach Spotify at all (connect failure, DNS, timeout) marks
//! the process offline; any response — even an HTTP error — marks it online again.
//! HTTP errors are *not* connectivity problems and never flip this.
//!
//! Callers use it to stand down (the shell stretches its polling into a backing-off
//! probe and shows one "Offline — reconnecting" line instead of an error per poll),
//! and to recognise a failure as "offline" by its message prefix.

use std::sync::atomic::{AtomicBool, Ordering};

static OFFLINE: AtomicBool = AtomicBool::new(false);

/// Prefix of every error message produced by an unreachable network.
pub const OFFLINE_PREFIX: &str = "Offline \u{2014} ";

/// A request couldn't reach Spotify.
pub fn note_down() {
    OFFLINE.store(true, Ordering::Relaxed);
}

/// A request got a response. Returns true if this ended an offline spell.
pub fn note_up() -> bool {
    OFFLINE.swap(false, Ordering::Relaxed)
}

pub fn is_offline() -> bool {
    OFFLINE.load(Ordering::Relaxed)
}

/// Was this error message produced by an unreachable network?
pub fn is_offline_error(msg: &str) -> bool {
    msg.starts_with(OFFLINE_PREFIX)
}

/// Is this transport error a connectivity failure (as opposed to, say, a body that
/// failed to decode)?
pub fn is_connectivity(e: &reqwest::Error) -> bool {
    e.is_connect() || e.is_timeout()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn down_then_up_reports_the_recovery_once() {
        note_down();
        assert!(is_offline());
        assert!(note_up());
        assert!(!is_offline());
        assert!(!note_up());
        assert!(is_offline_error(&format!("{OFFLINE_PREFIX}GET me/player: dns")));
        assert!(!is_offline_error("GET me/player 500"));
    }
}
