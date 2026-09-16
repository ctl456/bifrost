//! The pre-flight that announces a key to the upstream.
//!
//! Before a key is used to generate anything, the upstream is told two things
//! about it: which device it is on, and that a session exists. Both are records
//! rather than requests — the answer does not change what the generate does —
//! and both are per key rather than per request, so they are sent on the first
//! turn of a window and then left alone.
//!
//! This is also the only place the device identity ever goes. A generate envelope
//! carries the working directory and the environment, but not the fingerprint:
//! the record endpoint is where the upstream is asked to remember a machine, and
//! a deployment that never calls it has no fingerprint as far as the upstream is
//! concerned. That is why `mechanisms.fingerprint` is a switch on this path
//! rather than on the derivation, which is deterministic either way.
//!
//! Nothing here may fail a turn. A pre-flight that cannot be delivered is logged
//! and the window is left to come around again: the traffic it introduces the key
//! to is worth more than the introduction.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::core::Error;
use crate::fingerprint::generate_fingerprint;
use crate::wire::{Entropy, WireContext, WireRequest};

use crate::edge::log::warn;
use crate::edge::state::Edge;
use crate::edge::upstream;

/// How long an announcement stays good for.
///
/// Eight hours, as in the original: long enough that a key announces itself a few
/// times a day instead of on every request, short enough that a key which has been
/// quiet is announced again before the upstream would have cause to forget it.
const REFRESH: Duration = Duration::from_secs(8 * 60 * 60);

/// How much the next announcement is spread by.
///
/// Instances started together, and keys that first arrive together, would
/// otherwise announce again at the same instant. The jitter is what keeps a fleet
/// from being a spike.
const JITTER: Duration = Duration::from_secs(2 * 60 * 60);

/// When each key is next due to announce itself.
///
/// Per key, because what is announced is a key's device and session rather than
/// anything about the process. A key that has been quiet for a window has its
/// entry dropped, and loses nothing by it: an entry that has come due announces
/// in exactly the way a fresh one does.
#[derive(Debug, Default)]
pub struct Schedule {
    keys: Mutex<HashMap<String, Instant>>,
}

impl Schedule {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim this key's announcement, if one has come due.
    ///
    /// The window moves forward here, before the announcement is attempted, so
    /// that requests arriving together claim it once between them instead of each
    /// sending the same pair. [`Schedule::finish`] replaces the provisional window
    /// with the jittered one.
    ///
    /// `now` is a parameter rather than a call to `Instant::now` so that a test can
    /// walk a window without waiting eight hours for one.
    pub fn begin(&self, api_key: &str, now: Instant) -> bool {
        let mut keys = self.keys.lock().expect("lifecycle schedule poisoned");
        if keys.get(api_key).is_some_and(|next| now < *next) {
            return false;
        }
        keys.retain(|_, next| now < *next);
        keys.insert(api_key.to_owned(), now + REFRESH);
        true
    }

    /// Open the next window, once the announcement has been attempted.
    ///
    /// Called whether or not the attempt was delivered, which is what the original
    /// does and what a retry storm argues for: a key whose announcement failed is
    /// no more likely to succeed on the next request, and every request after it
    /// would try again.
    pub fn finish(&self, api_key: &str, now: Instant, jitter: Duration) {
        let mut keys = self.keys.lock().expect("lifecycle schedule poisoned");
        if let Some(next) = keys.get_mut(api_key) {
            *next = now + REFRESH + jitter;
        }
    }
}

/// How long until the announcement after this one.
///
/// Drawn from the shared entropy source rather than a second one, so that two
/// mechanisms cannot mint the same value by both counting from zero.
#[must_use]
pub fn window(entropy: &dyn Entropy) -> Duration {
    let mut jitter = [0u8; 4];
    entropy.fill(&mut jitter);
    REFRESH + Duration::from_secs(u64::from(u32::from_le_bytes(jitter)) % JITTER.as_secs())
}

/// Announce `api_key` to the upstream, if a window has come around.
///
/// Awaited by the turn that triggers it, because the point of the record is that
/// the upstream knows the device *before* it is asked to generate. That makes the
/// first request of a window pay for the introduction, which is the price of the
/// mechanism; every request after it pays nothing.
pub async fn announce(edge: &Edge, api_key: &str, context: &WireContext) {
    if !edge.config().mechanisms.lifecycle || !edge.schedule().begin(api_key, Instant::now()) {
        return;
    }

    let fingerprint = edge
        .config()
        .mechanisms
        .fingerprint
        .then(|| generate_fingerprint(api_key, &edge.config().fingerprint.salt, &context.device));
    let requests = edge.wire().announce(context, fingerprint.as_ref(), edge.entropy());

    // In parallel, as the original sends them: the two calls are independent, and
    // the first turn of a window should not wait for them end to end.
    let deliveries = requests.iter().map(|request| deliver(edge, request));
    futures_util::future::join_all(deliveries).await;

    edge.schedule().finish(api_key, Instant::now(), window(edge.entropy()));
}

/// Deliver one announcement, and say nothing if it does not arrive.
///
/// The status is checked but not acted on: the upstream answers these with what it
/// likes, and a rejection is the upstream's opinion about a record, not a reason to
/// refuse the turn that follows.
///
/// The answer's body is left unread and the connection closed with it. Nothing in
/// it is used, and reading it would be one more thing that can stall — an
/// announcement happens once per key per window, so the connection it costs is not
/// worth an unread body's worth of risk.
async fn deliver(edge: &Edge, request: &WireRequest) {
    let limit = edge.config().limits.announce();
    let attempt = async {
        let upstream = upstream::send(edge, request).await?;
        Ok::<u16, Error>(upstream.status)
    };

    match tokio::time::timeout(limit, attempt).await {
        Ok(Ok(status)) if (200..300).contains(&status) => {}
        Ok(Ok(status)) => {
            warn(format!(
                "upstream refused the {} announcement with status {status}",
                request.path
            ));
        }
        Ok(Err(error)) => {
            warn(format!("could not deliver the {} announcement: {error}", request.path));
        }
        Err(_elapsed) => {
            warn(format!(
                "the {} announcement did not answer within {}ms",
                request.path,
                limit.as_millis()
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The schedule is asked about, and answers about, one key at a time.
    #[test]
    fn a_key_announces_once_a_window() {
        let schedule = Schedule::new();
        let start = Instant::now();
        let eight_hours = Duration::from_secs(8 * 60 * 60);

        assert!(schedule.begin("key-a", start), "nothing has announced this key yet");
        assert!(
            !schedule.begin("key-a", start + Duration::from_secs(60)),
            "a key that has just announced does not announce again"
        );
        schedule.finish("key-a", start, Duration::ZERO);
        assert!(!schedule.begin("key-a", start + eight_hours - Duration::from_secs(1)));
        assert!(
            schedule.begin("key-a", start + eight_hours),
            "the window is over, so the key announces again"
        );
    }

    /// One key's window is not another's.
    #[test]
    fn keys_announce_independently() {
        let schedule = Schedule::new();
        let start = Instant::now();

        assert!(schedule.begin("key-a", start));
        assert!(schedule.begin("key-b", start));
        assert!(!schedule.begin("key-b", start + Duration::from_secs(1)));
    }

    /// The jitter is what spreads the next window, so it has to be part of it.
    #[test]
    fn the_jitter_delays_the_next_window() {
        let schedule = Schedule::new();
        let start = Instant::now();
        let jitter = Duration::from_secs(90 * 60);
        let eight_hours = Duration::from_secs(8 * 60 * 60);

        assert!(schedule.begin("key-a", start));
        schedule.finish("key-a", start, jitter);
        assert!(
            !schedule.begin("key-a", start + eight_hours),
            "eight hours alone is not the whole window once jitter is drawn"
        );
        assert!(schedule.begin("key-a", start + eight_hours + jitter));
    }

    /// A window that was claimed and never finished still holds: the pair was sent.
    #[test]
    fn a_claimed_window_is_not_reopened_by_the_next_request() {
        let schedule = Schedule::new();
        let start = Instant::now();

        assert!(schedule.begin("key-a", start));
        assert!(
            !schedule.begin("key-a", start + Duration::from_secs(30)),
            "a request arriving during the first one's announcement does not send a second"
        );
    }
}
