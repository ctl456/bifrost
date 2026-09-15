//! Archival, verification and journaling.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use bifrost_audit::{
    ArchiveStore, Held, Journal, KIND_REQUEST, Retention, contains_likely_secret, count_lines, digest_of, prune,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("bifrost-audit-{}-{}-{}", std::process::id(), label, unique));
    let _ = fs::remove_dir_all(&path);
    path
}

#[test]
fn a_stored_blob_reports_its_digest_size_and_line_count() {
    let store = ArchiveStore::new(temp_dir("put"));
    let body = b"alpha\nbeta\n";
    let reference = store.put(KIND_REQUEST, body).expect("store");

    assert_eq!(reference.sha256, digest_of(body));
    assert_eq!(reference.bytes, body.len() as u64);
    assert_eq!(reference.lines, 2);
    assert_eq!(store.read(KIND_REQUEST, &reference).expect("read"), body);
}

#[test]
fn identical_bytes_are_stored_once() {
    let root = temp_dir("dedupe");
    let store = ArchiveStore::new(&root);
    let first = store.put(KIND_REQUEST, b"same").expect("first");
    let second = store.put(KIND_REQUEST, b"same").expect("second");

    assert_eq!(first, second);
    let stored = fs::read_dir(root.join(KIND_REQUEST)).expect("list").count();
    assert_eq!(stored, 1, "content addressing must deduplicate");
}

/// A digest is the only name a journal line keeps for the bytes of a turn, so a
/// reader holding a line and no store has to be told which of three things happened:
/// the bytes are here, they were never here, or what is here is not them.
#[test]
fn a_digest_is_answered_with_what_is_actually_under_it() {
    let root = temp_dir("held");
    let store = ArchiveStore::new(&root);
    let reference = store.put(KIND_REQUEST, b"the turn's bytes").expect("store");

    assert_eq!(
        store.held(&reference.sha256).expect("look up"),
        Held::Intact {
            kind: KIND_REQUEST.to_owned(),
            bytes: reference.bytes,
            lines: reference.lines,
        }
    );

    // Tampered with rather than missing: the file is there, and its name is a claim
    // about bytes that no longer holds.
    fs::write(store.path_for(KIND_REQUEST, &reference), b"something else entirely").expect("overwrite");
    assert_eq!(
        store.held(&reference.sha256).expect("look up"),
        Held::Tampered {
            kind: KIND_REQUEST.to_owned()
        }
    );

    // A digest nothing was stored under, which is also what a pruned blob looks like:
    // the journal still names it, and the archive has nothing under that name.
    let absent = digest_of(b"never stored");
    assert_eq!(store.held(&absent).expect("look up"), Held::Gone);

    // And a store that is not there at all is not an error to look in.
    let empty = ArchiveStore::new(root.join("nothing-here"));
    assert_eq!(empty.held(&absent).expect("look up"), Held::Gone);
}

/// Looking up something that is not a digest is a mistake in the argument, not an
/// answer about the archive.
#[test]
fn looking_up_something_that_is_not_a_digest_is_refused() {
    let store = ArchiveStore::new(temp_dir("held-bad"));
    for bad in ["", "abc", &"g".repeat(64), &"A".repeat(64)] {
        let error = store.held(bad).expect_err("must be refused");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{bad:?}");
    }
}

/// Say that a blob was written `age` ago, so a retention test states an age rather
/// than waiting for a clock.
fn backdate(store: &ArchiveStore, kind: &str, reference: &bifrost_audit::ArchiveRef, age: std::time::Duration) {
    let file = fs::File::options()
        .write(true)
        .open(store.path_for(kind, reference))
        .expect("open");
    file.set_modified(std::time::SystemTime::now() - age)
        .expect("set mtime");
}

/// Retention from outside the crate: the store loses the expired bytes, the journal
/// keeps what it had and gains one line saying what the pass did.
#[test]
fn a_retention_pass_removes_the_expired_and_records_itself() {
    let root = temp_dir("retention");
    let store = ArchiveStore::new(root.join("archive"));
    let journal = Journal::new(root.join("journal.jsonl"));
    let week = std::time::Duration::from_secs(7 * 24 * 60 * 60);

    let expired = store.put(KIND_REQUEST, b"a question from last month").expect("store");
    let recent = store.put(KIND_REQUEST, b"a question from today").expect("store");
    backdate(&store, KIND_REQUEST, &expired, week * 8);

    let report = prune(
        &store,
        &journal,
        &Retention {
            max_age: Some(week),
            max_bytes: None,
        },
        std::time::SystemTime::now(),
    )
    .expect("prune");

    assert_eq!(report.removed, 1);
    assert_eq!(report.bytes, expired.bytes);
    assert!(
        store.read(KIND_REQUEST, &recent).is_ok(),
        "the recent bytes are still there"
    );
    assert!(
        store.read(KIND_REQUEST, &expired).is_err(),
        "the expired bytes are gone, and the store says so rather than serving them"
    );
    let entries = journal.read_all().expect("read journal");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["kind"], "retention");
}

#[test]
fn a_tampered_blob_is_detected_on_read() {
    let store = ArchiveStore::new(temp_dir("tamper"));
    let reference = store.put(KIND_REQUEST, b"original").expect("store");
    fs::write(store.path_for(KIND_REQUEST, &reference), b"tampered").expect("overwrite");

    let error = store.read(KIND_REQUEST, &reference).expect_err("tamper must be caught");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn quotes_are_matched_byte_for_byte() {
    let store = ArchiveStore::new(temp_dir("quote"));
    let reference = store
        .put(KIND_REQUEST, b"error: unsolved goals\n  goal 1\n")
        .expect("store");

    assert!(
        store
            .verify_quote(KIND_REQUEST, &reference, b"unsolved goals")
            .expect("verify")
    );
    assert!(store.verify_quote(KIND_REQUEST, &reference, b"goal 1").expect("verify"));
    assert!(
        !store
            .verify_quote(KIND_REQUEST, &reference, b"UNSOLVED GOALS")
            .expect("verify")
    );
    assert!(
        !store
            .verify_quote(KIND_REQUEST, &reference, b"a goal was omitted")
            .expect("verify")
    );
}

#[test]
fn an_empty_quote_is_never_evidence() {
    let store = ArchiveStore::new(temp_dir("empty"));
    let reference = store.put(KIND_REQUEST, b"anything").expect("store");
    assert!(!store.verify_quote(KIND_REQUEST, &reference, b"").expect("verify"));
}

#[test]
fn a_tampered_blob_cannot_validate_a_claim() {
    let store = ArchiveStore::new(temp_dir("tamper-quote"));
    let reference = store.put(KIND_REQUEST, b"real output").expect("store");
    fs::write(store.path_for(KIND_REQUEST, &reference), b"invented output").expect("overwrite");

    assert!(
        !store
            .verify_quote(KIND_REQUEST, &reference, b"invented output")
            .expect("verify"),
        "a blob that no longer matches its digest must never validate"
    );
}

#[test]
fn a_missing_blob_is_an_error_not_a_silent_match() {
    let store = ArchiveStore::new(temp_dir("missing"));
    let reference = store.put(KIND_REQUEST, b"present").expect("store");
    fs::remove_file(store.path_for(KIND_REQUEST, &reference)).expect("remove");

    let error = store
        .verify_quote(KIND_REQUEST, &reference, b"present")
        .expect_err("must surface");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn unsafe_archive_kinds_are_rejected() {
    let store = ArchiveStore::new(temp_dir("kinds"));
    for kind in ["../escape", "a/b", "", "a.b", "a b"] {
        let error = store.put(kind, b"x").expect_err("kind must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "kind {kind:?}");
    }
    store
        .put("upstream-response", b"x")
        .expect("dashes and underscores are fine");
}

#[test]
fn the_journal_appends_one_envelope_per_decision() {
    let journal = Journal::new(temp_dir("journal").join("decisions.jsonl"));
    journal
        .append("request", serde_json::json!({ "wire": "cc/1.53.1" }))
        .expect("append");
    journal
        .append("fallback", serde_json::json!({ "reason": "hash mismatch" }))
        .expect("append");

    let entries = journal.read_all().expect("read");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["kind"], "request");
    assert_eq!(entries[0]["data"]["wire"], "cc/1.53.1");
    assert_eq!(entries[1]["kind"], "fallback");
    assert!(
        entries[0]["timestamp"]
            .as_str()
            .is_some_and(|value| value.contains('T')),
        "entries carry an RFC 3339 timestamp"
    );
    assert_eq!(
        journal.read_all().expect("re-read").len(),
        2,
        "appending must not truncate"
    );
}

#[test]
fn journal_data_cannot_shadow_the_envelope() {
    let journal = Journal::new(temp_dir("journal-shadow").join("d.jsonl"));
    journal
        .append(
            "request",
            serde_json::json!({ "kind": "not-really", "timestamp": "not-really" }),
        )
        .expect("append");

    let entry = journal.read_all().expect("read").remove(0);
    assert_eq!(entry["kind"], "request");
    assert_ne!(entry["timestamp"], "not-really");
    assert_eq!(entry["data"]["kind"], "not-really");
}

#[test]
fn line_counting_treats_a_trailing_newline_as_a_terminator() {
    assert_eq!(count_lines(b""), 0);
    assert_eq!(count_lines(b"a"), 1);
    assert_eq!(count_lines(b"a\n"), 1);
    assert_eq!(count_lines(b"a\nb"), 2);
    assert_eq!(count_lines(b"a\nb\n"), 2);
    assert_eq!(count_lines(b"\n"), 1);
}

#[test]
fn credential_shaped_text_is_flagged() {
    assert!(contains_likely_secret("Authorization: Bearer abc123"));
    assert!(contains_likely_secret("api_key=sk-live-123"));
    assert!(contains_likely_secret("apikey: 1234"));
    assert!(contains_likely_secret("access_token: xyz"));
    assert!(contains_likely_secret("secret = hunter2"));
    assert!(contains_likely_secret("X-SECRET: value"));
}

#[test]
fn ordinary_diagnostics_are_not_flagged() {
    assert!(!contains_likely_secret("error: unsolved goals"));
    assert!(!contains_likely_secret("the build failed after 12 seconds"));
    assert!(!contains_likely_secret(""));
    assert!(!contains_likely_secret("secret"));
}

#[test]
fn the_flag_is_scanning_for_assignments_not_substrings() {
    assert!(!contains_likely_secret(
        "the word bearer appears here with no assignment"
    ));
    assert!(contains_likely_secret("bearer token is used as follows: value"));
}
