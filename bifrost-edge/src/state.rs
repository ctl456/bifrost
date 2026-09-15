//! Everything one running edge holds.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bifrost_config::Config;
use bifrost_core::Error;
use bifrost_fingerprint::DeviceProfile;
use bifrost_protocol::adapter::{ConvertOptions, ProtocolAdapter, registry};
use bifrost_wire::{Entropy, SystemEntropy, WireAdapter, WireContext, adapter_for, uuid_v4};

use crate::evidence::Evidence;
use crate::lifecycle::Schedule;
use crate::models::Catalog;

/// How long a generated session id stays usable.
///
/// The original's 12 hours plus up to an hour of jitter, so that a fleet of
/// instances started together does not rotate every session at the same moment.
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const SESSION_JITTER: Duration = Duration::from_secs(60 * 60);

/// A session id and the moment it stops being one.
#[derive(Debug, Clone)]
struct Session {
    id: String,
    expires_at: u64,
}

/// Headers a client may name its own session with, most specific first.
///
/// The last entry is not a header: OpenAI's `prompt_cache_key` identifies a
/// conversation, which is what a session is here, so it is accepted as one.
const SESSION_HEADERS: &[&str] = &["x-session-id", "x-claude-code-session-id", "session_id"];

/// The shortest value accepted as a session id.
const MIN_SESSION_LEN: usize = 8;

/// Shared state behind every handler.
pub struct Edge {
    config: Config,
    adapters: Vec<Box<dyn ProtocolAdapter>>,
    wire: Box<dyn WireAdapter>,
    client: reqwest::Client,
    entropy: SystemEntropy,
    device: DeviceProfile,
    sessions: Mutex<HashMap<String, Session>>,
    /// Requests in flight, for the optional ceiling.
    inflight: AtomicUsize,
    /// Idle timeouts since the last turn that finished, which decide how the next
    /// one is worded.
    timeouts: AtomicU32,
    /// What this process has answered so far.
    counters: Counters,
    /// When it started, which is the only way to answer how long it has been up.
    started: Instant,
    /// The audit record, when this deployment keeps one.
    evidence: Option<Arc<Evidence>>,
    /// When each key is next due to announce itself to the upstream.
    schedule: Schedule,
    /// The model catalogue, and when the copy of it was fetched.
    catalog: Catalog,
}

impl Edge {
    /// Build the edge, refusing to start on an unknown wire dialect.
    ///
    /// Falling back to a version this build does not implement would mean
    /// silently speaking a dialect the operator did not ask for.
    pub fn new(config: Config) -> Result<Arc<Self>, String> {
        let id = config
            .wire
            .parse_adapter()
            .ok_or_else(|| format!("malformed wire adapter id `{}`", config.wire.adapter))?;
        let wire = adapter_for(&id).ok_or_else(|| {
            format!(
                "unknown wire adapter `{}/{}`; this build speaks {}",
                id.name,
                id.version,
                bifrost_wire::SUPPORTED.join(", ")
            )
        })?;

        // Built once and never probed: an archive directory that cannot be
        // created is a problem the first write reports, not a reason to refuse
        // traffic that the operator asked to serve.
        let evidence = config.mechanisms.evidence_archive.then(|| {
            Arc::new(Evidence::new(
                config.audit.archive_dir.clone(),
                config.audit.journal_dir.clone(),
            ))
        });

        // One profile, built once: the fingerprint, the envelope's environment and
        // working directory, the project slug and the lifecycle metadata are all
        // read off this, so none of them can disagree with another.
        let device = match &config.device.project_dir {
            Some(dir) => DeviceProfile {
                project_dir: dir.clone(),
                ..DeviceProfile::default()
            },
            None => DeviceProfile::default(),
        };

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .map_err(|error| format!("could not build the upstream client: {error}"))?;

        Ok(Arc::new(Self {
            config,
            adapters: registry(),
            wire,
            client,
            entropy: SystemEntropy::new(),
            device,
            sessions: Mutex::new(HashMap::new()),
            inflight: AtomicUsize::new(0),
            timeouts: AtomicU32::new(0),
            counters: Counters::default(),
            started: Instant::now(),
            evidence,
            schedule: Schedule::new(),
            catalog: Catalog::new(),
        }))
    }

    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    #[must_use]
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Find an adapter by the name it answers to.
    ///
    /// The registry is a list rather than a map because it is built once and
    /// holds three entries; a lookup is a walk of three pointers.
    #[must_use]
    pub fn adapter(&self, name: &str) -> Option<&dyn ProtocolAdapter> {
        self.adapters
            .iter()
            .find(|adapter| adapter.name() == name)
            .map(AsRef::as_ref)
    }

    /// The dialect this deployment speaks upstream.
    #[must_use]
    pub(crate) fn wire(&self) -> &dyn WireAdapter {
        self.wire.as_ref()
    }

    /// The random source every mechanism draws on.
    ///
    /// Shared rather than handed out per mechanism: a second source would count
    /// from zero again, and the counter is part of what keeps two values apart.
    #[must_use]
    pub(crate) fn entropy(&self) -> &dyn Entropy {
        &self.entropy
    }

    /// When each key is next due to announce itself.
    #[must_use]
    pub(crate) fn schedule(&self) -> &Schedule {
        &self.schedule
    }

    /// The cached model catalogue.
    #[must_use]
    pub(crate) fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// The conversion knobs this deployment configured.
    #[must_use]
    pub fn convert_options(&self) -> ConvertOptions {
        ConvertOptions {
            synthesize_cache_breakpoint: self.config.mechanisms.prompt_cache,
        }
    }

    /// Where this deployment keeps its audit record, if it keeps one.
    #[must_use]
    pub fn evidence(&self) -> Option<&Arc<Evidence>> {
        self.evidence.as_ref()
    }

    /// The session a request is filed under.
    ///
    /// Separate from [`Edge::wire_context`] so the audit record can name the same
    /// session the upstream was told, rather than rebuilding it from the envelope
    /// or minting a second one.
    #[must_use]
    pub fn session_for(
        &self,
        api_key: &str,
        headers: &axum::http::HeaderMap,
        prompt_cache_key: Option<&str>,
    ) -> String {
        self.session_id(api_key, headers, prompt_cache_key)
    }

    /// The context one upstream request is encoded with.
    #[must_use]
    pub fn wire_context(&self, api_key: &str, session_id: &str) -> WireContext {
        let context = WireContext::new(
            api_key,
            session_id.to_owned(),
            self.device.clone(),
            bifrost_wire::today_utc(),
            &self.entropy,
        );
        context.with_zdr(self.config.wire.zdr)
    }

    /// Encode a canonical request for the configured upstream dialect.
    pub fn encode(
        &self,
        request: &bifrost_core::CanonicalRequest,
        context: &WireContext,
    ) -> Result<bifrost_wire::WireRequest, bifrost_core::ConversionError> {
        self.wire.encode(request, context)
    }

    /// The session id to report upstream.
    ///
    /// A client that names its own session keeps it: it is describing the same
    /// conversation the client is, and replacing it would defeat the point.
    ///
    /// A deployment that does not report sessions reports none of them, wherever
    /// the id would have come from — the client's header included, since the
    /// switch is about what leaves this process. Nothing is minted either, so a
    /// deployment that never reads the store does not grow one.
    fn session_id(&self, api_key: &str, headers: &axum::http::HeaderMap, prompt_cache_key: Option<&str>) -> String {
        if !self.config.mechanisms.session {
            return String::new();
        }
        for name in SESSION_HEADERS {
            if let Some(value) = headers.get(*name).and_then(|value| value.to_str().ok())
                && value.len() >= MIN_SESSION_LEN
            {
                return value.to_owned();
            }
        }
        if let Some(key) = prompt_cache_key
            && key.len() >= MIN_SESSION_LEN
        {
            return key.to_owned();
        }
        self.ensure_session(api_key)
    }

    /// A stable per-key session, minted once and reused until it expires.
    fn ensure_session(&self, api_key: &str) -> String {
        let now = now_unix();
        let mut sessions = self.sessions.lock().expect("session store poisoned");

        if let Some(session) = sessions.get(api_key)
            && now < session.expires_at
        {
            return session.id.clone();
        }

        let id = uuid_v4(&self.entropy);
        let mut jitter = [0u8; 4];
        self.entropy.fill(&mut jitter);
        let jitter = Duration::from_secs(u64::from(u32::from_le_bytes(jitter)) % SESSION_JITTER.as_secs());
        sessions.insert(
            api_key.to_owned(),
            Session {
                id: id.clone(),
                expires_at: now + SESSION_TTL.as_secs() + jitter.as_secs(),
            },
        );
        sessions.retain(|_, session| now < session.expires_at);
        id
    }

    /// Admit a request against the in-flight ceiling, if one is configured.
    ///
    /// The permit releases when it is dropped, so every exit path — success,
    /// failure, client disconnect — returns the slot exactly once.
    pub fn admit(self: &Arc<Self>) -> Option<InflightPermit> {
        let ceiling = self.config.limits.max_inflight;
        if ceiling == 0 {
            return None;
        }
        if self.inflight.fetch_add(1, Ordering::AcqRel) >= ceiling as usize {
            self.inflight.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(InflightPermit(Arc::clone(self)))
    }

    #[must_use]
    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::Acquire)
    }

    /// Count an idle timeout and report how many have happened in a row.
    ///
    /// The run is gated here rather than at the call site, so a deployment that did
    /// not ask for the hint is not told about one. The total is not gated: how often
    /// the upstream goes quiet is worth being able to ask whether or not the client
    /// is handed a differently worded message.
    pub fn note_timeout(&self) -> u32 {
        self.counters.timeouts.fetch_add(1, Ordering::Relaxed);
        if !self.config.mechanisms.timeout_context_hint {
            return 0;
        }
        self.timeouts.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// A turn the upstream answered: forget the run of timeouts, and count it.
    ///
    /// "In a row" is the whole point — the hint that follows is about a
    /// conversation that keeps exceeding the window, which a run of unrelated
    /// slow turns is not.
    pub fn note_success(&self) {
        self.counters.turns.fetch_add(1, Ordering::Relaxed);
        if self.config.mechanisms.timeout_context_hint {
            self.timeouts.store(0, Ordering::Release);
        }
    }

    /// Count a turn refused at the in-flight ceiling.
    pub fn note_refused(&self) {
        self.counters.refused.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a request that arrived without a key.
    pub fn note_unauthenticated(&self) {
        self.counters.unauthenticated.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a body over the configured ceiling.
    pub fn note_too_large(&self) {
        self.counters.too_large.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a body that was not the JSON this protocol asks for.
    pub fn note_malformed(&self) {
        self.counters.malformed.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a turn the upstream could not answer: a refusal, a connection that
    /// broke, or an answer that carried no output.
    pub fn note_upstream_failure(&self) {
        self.counters.upstream_failed.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a stream cut because its client stopped reading it.
    pub fn note_client_stall(&self) {
        self.counters.client_stalls.fetch_add(1, Ordering::Relaxed);
    }

    /// What this process has been doing since it started.
    ///
    /// Aggregate on purpose: no key fingerprints, no model names, no bodies. It is
    /// there to tell "nothing is arriving" from "nothing is working" without
    /// anyone parsing a log, and it is read to be reported rather than acted on,
    /// which is why the loads are `Relaxed`: no other value is ordered against
    /// these.
    #[must_use]
    pub fn status(&self) -> Status {
        let counters = &self.counters;
        Status {
            uptime_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            inflight: self.inflight(),
            max_inflight: self.config.limits.max_inflight,
            turns: counters.turns.load(Ordering::Relaxed),
            refused: counters.refused.load(Ordering::Relaxed),
            unauthenticated: counters.unauthenticated.load(Ordering::Relaxed),
            too_large: counters.too_large.load(Ordering::Relaxed),
            malformed: counters.malformed.load(Ordering::Relaxed),
            upstream_failed: counters.upstream_failed.load(Ordering::Relaxed),
            timeouts: counters.timeouts.load(Ordering::Relaxed),
            client_stalls: counters.client_stalls.load(Ordering::Relaxed),
        }
    }

    /// Word an idle timeout, hinting at the context only once it has repeated.
    ///
    /// One timeout is a slow turn; several in a row mean the prompt is probably
    /// too large for the model to answer inside the window, which the client can
    /// act on.
    #[must_use]
    pub fn timeout_message(&self) -> String {
        if self.config.mechanisms.timeout_context_hint && self.timeouts.load(Ordering::Acquire) >= TIMEOUT_HINT_AFTER {
            format!("{TIMEOUT_PREFIX} - try reducing context length (summarize earlier messages)")
        } else {
            format!("{TIMEOUT_PREFIX} - request timed out")
        }
    }
}

/// The counts behind [`Status`].
///
/// Private, and moved only through the methods on [`Edge`] that name the decision
/// being counted: a counter a call site could set for itself would end up meaning
/// whatever the last one to touch it thought.
#[derive(Debug, Default)]
struct Counters {
    turns: AtomicU64,
    refused: AtomicU64,
    unauthenticated: AtomicU64,
    too_large: AtomicU64,
    malformed: AtomicU64,
    upstream_failed: AtomicU64,
    timeouts: AtomicU64,
    client_stalls: AtomicU64,
}

/// What a running deployment has answered.
///
/// Every field is a count of one decision, so a reader can tell a deployment that
/// is being asked for nothing from one that is failing at everything, and can tell
/// a client sending broken requests from an upstream that stopped answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Status {
    /// How long this process has been running.
    pub uptime_ms: u64,
    /// Turns in flight right now.
    pub inflight: usize,
    /// The ceiling `inflight` is measured against; 0 means this deployment has none.
    pub max_inflight: u32,
    /// Turns the upstream answered.
    pub turns: u64,
    /// Turns refused at the in-flight ceiling.
    pub refused: u64,
    /// Requests that arrived without a key.
    pub unauthenticated: u64,
    /// Bodies over `limits.max_body_mb`.
    pub too_large: u64,
    /// Bodies that were not the JSON the protocol asks for.
    pub malformed: u64,
    /// Turns the upstream could not answer.
    pub upstream_failed: u64,
    /// Times the upstream went quiet for longer than the configured window.
    pub timeouts: u64,
    /// Streams cut because the client stopped reading them.
    pub client_stalls: u64,
}

/// How many consecutive timeouts pass before the client is told to shorten the
/// conversation.
const TIMEOUT_HINT_AFTER: u32 = 3;

/// Holds one in-flight slot for as long as it lives.
pub struct InflightPermit(Arc<Edge>);

impl std::fmt::Debug for InflightPermit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InflightPermit")
            .field("inflight", &self.0.inflight())
            .finish()
    }
}

impl Drop for InflightPermit {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The words every idle timeout starts with.
///
/// The shared error surface has no field for "this one was an idle timeout", and
/// adding one would put a mechanism's vocabulary into the type all three
/// protocols use. The sentence is the marker instead.
pub const TIMEOUT_PREFIX: &str = "Response timeout";

/// Whether a failure is the upstream going quiet.
#[must_use]
pub fn is_idle_timeout(error: &Error) -> bool {
    error.message.starts_with(TIMEOUT_PREFIX)
}

/// The failure reported when the upstream goes quiet for too long.
///
/// Retryable: the request may well work on a second attempt, and a client that
/// retries without a hint hammers at full speed.
#[must_use]
pub fn idle_timeout_error() -> Error {
    Error {
        retry_after: Some(5),
        ..Error::rate_limit(format!("{TIMEOUT_PREFIX} - request timed out"))
    }
}

/// The failure reported when a turn produced nothing billable.
///
/// A turn that produced nothing is a failed turn: billing it as a completion
/// would let a client treat silence as an answer.
#[must_use]
pub fn empty_output_error() -> Error {
    Error {
        retry_after: Some(10),
        ..Error::rate_limit("Empty response from upstream (zero output tokens)")
    }
}

/// Seconds since the epoch.
#[must_use]
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bifrost_config::MechanismsConfig;

    fn edge(timeout_context_hint: bool) -> Arc<Edge> {
        let config = Config {
            mechanisms: MechanismsConfig {
                timeout_context_hint,
                ..Default::default()
            },
            ..Config::default()
        };
        Edge::new(config).expect("the default dialect is known")
    }

    #[tokio::test]
    async fn the_context_hint_waits_for_a_run_and_a_finished_turn_breaks_it() {
        let edge = edge(true);

        assert_eq!(edge.note_timeout(), 1);
        assert_eq!(edge.note_timeout(), 2);
        assert!(edge.timeout_message().contains("request timed out"));

        assert_eq!(edge.note_timeout(), 3);
        assert!(
            edge.timeout_message().contains("reducing context"),
            "three in a row is a conversation that does not fit, not three slow turns"
        );

        edge.note_success();
        assert_eq!(edge.note_timeout(), 1, "the run starts over after a turn that worked");
        assert!(edge.timeout_message().contains("request timed out"));
    }

    /// Each count moves for the thing it names and for nothing else, which is the
    /// whole value of reporting them: a number that moves for two reasons says
    /// neither.
    #[test]
    fn every_count_is_its_own() {
        let edge = edge(true);
        let fresh = edge.status();
        assert_eq!(fresh, Status::default(), "a process that has done nothing");

        edge.note_success();
        edge.note_refused();
        edge.note_unauthenticated();
        edge.note_too_large();
        edge.note_malformed();
        edge.note_upstream_failure();
        edge.note_client_stall();
        edge.note_timeout();

        let status = edge.status();
        assert_eq!(status.turns, 1);
        assert_eq!(status.refused, 1);
        assert_eq!(status.unauthenticated, 1);
        assert_eq!(status.too_large, 1);
        assert_eq!(status.malformed, 1);
        assert_eq!(status.upstream_failed, 1);
        assert_eq!(status.client_stalls, 1);
        assert_eq!(status.timeouts, 1);
        assert_eq!(status.inflight, 0);
    }

    /// The client is only handed an edited message when the hint is on; the count
    /// is kept either way, because the question it answers is about the upstream.
    #[test]
    fn a_timeout_is_counted_with_the_hint_off_too() {
        let edge = edge(false);
        assert_eq!(edge.note_timeout(), 0, "the hint is off, so no run is kept");
        assert_eq!(edge.status().timeouts, 1);
    }

    #[test]
    fn uptime_is_measured_from_the_start() {
        let edge = edge(true);
        std::thread::sleep(Duration::from_millis(2));
        assert!(edge.status().uptime_ms >= 2, "{}", edge.status().uptime_ms);
    }

    #[tokio::test]
    async fn a_deployment_without_the_hint_neither_counts_nor_hints() {
        let edge = edge(false);

        for _ in 0..5 {
            assert_eq!(edge.note_timeout(), 0);
        }
        edge.note_success();
        assert!(edge.timeout_message().contains("request timed out"));
    }
}
