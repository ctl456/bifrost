//! The evidence-preserving mechanism.
//!
//! The other opt-in mechanisms change the traffic: a fingerprint and a session id
//! are things the upstream would not otherwise have been told. This one changes
//! nothing. It keeps the original bytes, so that a later claim about them — "the
//! client sent forty thousand tokens", "the model refused", "the turn produced
//! nothing" — can be checked instead of believed.
//!
//! It is off by default because it writes to disk, and it never fails a request:
//! a full disk costs the operator their evidence, not their traffic.

use std::io;
use std::path::{Path, PathBuf};

use bifrost_audit::{ArchiveRef, ArchiveStore, Journal, Pruned, Retention, contains_likely_secret, digest_of};

/// How many bytes of a streamed answer are kept.
///
/// The store is content-addressed, so a blob is written in one call and has to be
/// whole before it is. A stream longer than this is recorded as truncated rather
/// than growing the process without bound.
pub const CAPTURE_CAP: usize = 8 * 1024 * 1024;

/// The journal kind for one completed turn.
const KIND_TURN: &str = "turn";

/// How much of a key digest is kept. Sixteen hex characters identify a key
/// without being one, which is all the journal needs.
const KEY_FINGERPRINT_LEN: usize = 16;

/// Where one turn's original bytes went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    pub reference: ArchiveRef,
    /// Whether the bytes look like they embed a credential.
    pub sensitive: bool,
    /// Whether capture stopped at [`CAPTURE_CAP`] before the stream ended.
    pub truncated: bool,
}

/// What the journal records about one turn.
///
/// Deliberately metadata only. The bytes live in the archive under their digest,
/// and a log line that repeated them would deliver them to every reader of the
/// log.
#[derive(Debug, Clone)]
pub struct Turn {
    pub protocol: String,
    pub model: String,
    pub stream: bool,
    pub status: u16,
    pub session: String,
    /// A digest of the key the turn was billed to, never the key.
    pub key_fingerprint: String,
    /// Absent when the request could not be archived. Recorded as absent rather
    /// than omitted, so a reader can tell a missing half from a broken writer.
    pub request: Option<Captured>,
    pub response: Option<Captured>,
}

/// The half of a journal line that is known before the turn is sent.
#[derive(Debug, Clone)]
pub struct TurnContext {
    pub protocol: String,
    pub model: String,
    pub session: String,
    pub key_fingerprint: String,
}

impl TurnContext {
    /// The line for a turn that has reached `status`.
    #[must_use]
    pub fn turn(&self, status: u16, stream: bool, request: Option<Captured>, response: Option<Captured>) -> Turn {
        Turn {
            protocol: self.protocol.clone(),
            model: self.model.clone(),
            stream,
            status,
            session: self.session.clone(),
            key_fingerprint: self.key_fingerprint.clone(),
            request,
            response,
        }
    }
}

/// The archive and the journal, as one mechanism.
#[derive(Debug, Clone)]
pub struct Evidence {
    store: ArchiveStore,
    journal: Journal,
}

impl Evidence {
    #[must_use]
    pub fn new(archive_dir: impl Into<PathBuf>, journal_dir: impl Into<PathBuf>) -> Self {
        Self {
            store: ArchiveStore::new(archive_dir),
            journal: Journal::new(journal_dir),
        }
    }

    #[must_use]
    pub fn archive_dir(&self) -> &Path {
        self.store.root()
    }

    #[must_use]
    pub fn journal_path(&self) -> &Path {
        self.journal.path()
    }

    /// Keep a client's request body.
    pub fn archive_request(&self, body: &[u8]) -> io::Result<Captured> {
        self.capture(bifrost_audit::KIND_REQUEST, body, false)
    }

    /// Keep a complete upstream answer.
    pub fn archive_response(&self, body: &[u8]) -> io::Result<Captured> {
        self.capture(bifrost_audit::KIND_UPSTREAM_RESPONSE, body, false)
    }

    /// Keep the raw events of a streamed upstream answer.
    pub fn archive_events(&self, body: &[u8], truncated: bool) -> io::Result<Captured> {
        self.capture(bifrost_audit::KIND_UPSTREAM_EVENT, body, truncated)
    }

    /// Write the line describing a turn that has finished.
    pub fn record(&self, turn: &Turn) -> io::Result<()> {
        self.journal.append(KIND_TURN, line(turn))
    }

    /// Remove what the retention allows, recording the pass in the journal.
    ///
    /// The journal is passed along rather than left behind because it is what says
    /// whether a blob is still named by a turn the deployment asked to keep: the
    /// store deduplicates, so an old file can be the bytes of a recent turn.
    pub fn prune(&self, retention: &Retention, now: std::time::SystemTime) -> io::Result<Pruned> {
        bifrost_audit::prune(&self.store, &self.journal, retention, now)
    }

    fn capture(&self, kind: &str, body: &[u8], truncated: bool) -> io::Result<Captured> {
        Ok(Captured {
            reference: self.store.put(kind, body)?,
            sensitive: looks_sensitive(body),
            truncated,
        })
    }
}

/// A stable name for a key that is not the key.
///
/// The journal exists to be read — by an operator, by a script, by whoever
/// inherits the deployment — so it records which key a turn belonged to without
/// handing that key to every reader.
#[must_use]
pub fn key_fingerprint(api_key: &str) -> String {
    digest_of(api_key.as_bytes())[..KEY_FINGERPRINT_LEN].to_owned()
}

/// Take an archive result, reporting a failure instead of raising it.
///
/// An audit that cannot be written is not a reason to fail the traffic it was
/// meant to describe: the operator loses their evidence, finds out from the log,
/// and keeps serving.
#[must_use]
pub fn or_warn<T>(result: io::Result<T>, what: &str) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            crate::log::warn(format!("could not archive {what}: {error}"));
            None
        }
    }
}

/// Whether the bytes look like they embed a credential.
///
/// Only a flag: the archive's whole job is to keep the bytes as they were, so the
/// most it can do for the operator is tell them what they just kept.
fn looks_sensitive(body: &[u8]) -> bool {
    contains_likely_secret(&String::from_utf8_lossy(body))
}

fn line(turn: &Turn) -> serde_json::Value {
    serde_json::json!({
        "protocol": turn.protocol,
        "model": turn.model,
        "stream": turn.stream,
        "status": turn.status,
        "session": turn.session,
        "key": turn.key_fingerprint,
        "request": turn.request.as_ref().map(describe),
        "response": turn.response.as_ref().map(describe),
    })
}

fn describe(captured: &Captured) -> serde_json::Value {
    serde_json::json!({
        "sha256": captured.reference.sha256,
        "bytes": captured.reference.bytes,
        "lines": captured.reference.lines,
        "sensitive": captured.sensitive,
        "truncated": captured.truncated,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("bifrost-edge-evidence-{}-{label}-{unique}", std::process::id()))
    }

    fn evidence(label: &str) -> (Evidence, PathBuf) {
        let root = temp_dir(label);
        (Evidence::new(root.join("archive"), root.join("journal.jsonl")), root)
    }

    #[test]
    fn an_archived_body_reads_back_byte_for_byte() {
        let (evidence, _root) = evidence("bytes");
        // Not valid UTF-8 on purpose: an archive that decoded what it stored
        // would hand back a different body than the one it was given.
        let body = b"{\"text\":\"\xff\xfe\"}";

        let captured = evidence.archive_request(body).expect("archive");
        let stored = evidence
            .store
            .read(bifrost_audit::KIND_REQUEST, &captured.reference)
            .expect("read back");

        assert_eq!(stored, body);
        assert_eq!(captured.reference.bytes, body.len() as u64);
        assert_eq!(captured.reference.sha256, digest_of(body));
    }

    #[test]
    fn a_body_that_embeds_a_key_is_flagged_rather_than_rewritten() {
        let (evidence, _root) = evidence("sensitive");

        let plain = evidence
            .archive_request(b"{\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}")
            .expect("archive");
        let secret = evidence
            .archive_request(b"{\"api_key\": \"sk-live-1234\"}")
            .expect("archive");

        assert!(!plain.sensitive);
        assert!(secret.sensitive, "the guard is what tells an operator what they kept");
        assert_eq!(
            evidence
                .store
                .read(bifrost_audit::KIND_REQUEST, &secret.reference)
                .expect("read back"),
            b"{\"api_key\": \"sk-live-1234\"}",
            "flagging is not redacting"
        );
    }

    #[test]
    fn a_turn_is_recorded_against_the_digests_of_what_it_archived() {
        let (evidence, _root) = evidence("turn");
        let request = evidence.archive_request(b"{\"model\":\"m\"}").expect("archive");
        let response = evidence
            .archive_events(b"{\"type\":\"finish\"}\n", false)
            .expect("archive");

        evidence
            .record(&Turn {
                protocol: "openai-chat".to_owned(),
                model: "m".to_owned(),
                stream: true,
                status: 200,
                session: "session-abcdefgh".to_owned(),
                key_fingerprint: key_fingerprint("user_abcdef123"),
                request: Some(request),
                response: Some(response),
            })
            .expect("record");

        let entries = evidence.journal.read_all().expect("read journal");
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry["kind"], serde_json::json!(KIND_TURN));
        assert_eq!(entry["data"]["protocol"], serde_json::json!("openai-chat"));
        assert_eq!(entry["data"]["stream"], serde_json::json!(true));
        assert_eq!(
            entry["data"]["request"]["sha256"],
            serde_json::json!(digest_of(b"{\"model\":\"m\"}"))
        );
        assert_eq!(
            entry["data"]["response"]["sha256"],
            serde_json::json!(digest_of(b"{\"type\":\"finish\"}\n"))
        );
        assert_eq!(entry["data"]["request"]["sensitive"], serde_json::json!(false));
    }

    #[test]
    fn a_recorded_turn_never_carries_the_key_itself() {
        let (evidence, _root) = evidence("fingerprint");
        let api_key = "user_abcdef1234567890";
        let request = evidence.archive_request(b"{}").expect("archive");

        evidence
            .record(&Turn {
                protocol: "anthropic".to_owned(),
                model: "m".to_owned(),
                stream: false,
                status: 200,
                session: "session-abcdefgh".to_owned(),
                key_fingerprint: key_fingerprint(api_key),
                request: Some(request),
                response: None,
            })
            .expect("record");

        let text = std::fs::read_to_string(evidence.journal_path()).expect("read journal");
        assert!(
            !text.contains(api_key),
            "the journal is readable by anyone who can read the disk"
        );
        assert!(text.contains(&key_fingerprint(api_key)));
        // The absent half of a turn is spelled out, so a reader does not have to
        // tell "no response was kept" from "the field was dropped by a bug".
        assert!(text.contains("\"response\":null"));
    }
}
