//! Keeping the archive from growing without bound.
//!
//! The store is content-addressed and never overwrites, which is what makes a
//! handle verifiable for the life of the directory — and also what makes the
//! directory grow for the life of the deployment. Retention answers the second
//! half, and it is lossy by nature, so what a pass removed is written where it can
//! be read back: the journal gets a line of its own.
//!
//! Two rules keep it from destroying evidence that was asked to be kept:
//!
//! - The journal is never pruned. It is the index, and a line whose bytes are gone
//!   still says what happened; a journal with holes in it could not.
//! - A blob is kept when a journal line inside the retention window names it, even
//!   when the file itself is older. Identical bytes are stored once, so the file an
//!   old turn created can be the file yesterday's turn was made of.
//!
//! The two settings answer different questions, and which one wins is not a detail:
//! bytes inside the window are kept even when that exceeds the ceiling. The window
//! is what the operator said the evidence was worth, and the ceiling is the disk it
//! has to fit on — a pass that met the ceiling by deleting the window would be
//! quietly destroying what it was told to keep, so it reports the conflict instead.
//! With no window there is nothing to protect, and the ceiling takes what it needs.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::archive::{ArchiveStore, is_digest_name};
use crate::journal::Journal;

/// How long archived bytes are kept, and how many of them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Retention {
    /// Remove what was written longer ago than this. `None` keeps it forever.
    pub max_age: Option<Duration>,
    /// Remove the oldest unprotected bytes until at most this many are left.
    /// `None` is no ceiling.
    pub max_bytes: Option<u64>,
}

impl Retention {
    /// Whether this asks for nothing, in which case a pass has nothing to do.
    #[must_use]
    pub fn is_off(&self) -> bool {
        self.max_age.is_none() && self.max_bytes.is_none()
    }
}

/// What one pass removed, and what it could not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pruned {
    /// Blobs removed.
    pub removed: u64,
    /// Bytes those blobs held.
    pub bytes: u64,
    /// Blobs past the age that were kept because a journal line inside the window
    /// still names them.
    pub kept_alive: u64,
    /// Whether the ceiling is still exceeded: everything left is named by a line
    /// inside the window, so removing more would break evidence that was asked to
    /// be kept.
    pub over_cap: bool,
}

/// One archived file, as the walk found it.
#[derive(Debug)]
struct Blob {
    path: PathBuf,
    digest: String,
    bytes: u64,
    /// When the bytes were first stored, which is not when the turn that names
    /// them ran: identical bytes are stored once.
    written: SystemTime,
}

/// Remove what the retention allows, and record the pass in the journal.
///
/// The deletions happen first and the line describing them second, so a journal
/// that cannot be written is reported as a failed pass even though the bytes are
/// already gone — the alternative is a line claiming a removal that did not
/// happen.
///
/// Every pass leaves a line, including one that found nothing to remove and one
/// that found no store at all. The line is what tells an operator that pruning is
/// running: a pass that only spoke when it deleted something would leave a
/// deployment that is comfortably within its retention looking exactly like one
/// whose retention nobody wired up.
pub fn prune(store: &ArchiveStore, journal: &Journal, retention: &Retention, now: SystemTime) -> io::Result<Pruned> {
    // A cutoff the clock cannot represent means the allowance is longer than the
    // epoch, so nothing is old enough to be past it.
    let cutoff = retention
        .max_age
        .map(|age| now.checked_sub(age).unwrap_or(SystemTime::UNIX_EPOCH));
    let protected = match cutoff {
        Some(cutoff) => named_since(journal, cutoff)?,
        None => BTreeSet::new(),
    };

    let mut report = Pruned::default();
    // A deployment that archives nothing has no store to walk, which is a pass that
    // found nothing rather than a pass that did not happen.
    let mut blobs = if store.root().is_dir() {
        walk(store.root())?
    } else {
        Vec::new()
    };
    // Oldest first, so a ceiling takes from the far end and the order of two
    // removals never depends on the order the filesystem listed them in.
    blobs.sort_by(|left, right| {
        left.written
            .cmp(&right.written)
            .then_with(|| left.path.cmp(&right.path))
    });

    let mut kept: Vec<&Blob> = Vec::new();
    for blob in &blobs {
        if !cutoff.is_some_and(|cutoff| blob.written < cutoff) {
            kept.push(blob);
            continue;
        }
        if protected.contains(&blob.digest) {
            report.kept_alive += 1;
            kept.push(blob);
            continue;
        }
        remove(blob, &mut report)?;
    }

    // The ceiling, after the age: it is the backstop for what age cannot reach,
    // which is a window longer than the disk it is kept on.
    if let Some(cap) = retention.max_bytes {
        let mut total: u64 = kept.iter().map(|blob| blob.bytes).sum();
        for blob in &kept {
            if total <= cap {
                break;
            }
            if protected.contains(&blob.digest) {
                continue;
            }
            remove(blob, &mut report)?;
            total -= blob.bytes;
        }
        report.over_cap = total > cap;
    }

    journal.append(
        crate::KIND_RETENTION,
        serde_json::json!({
            "removed": report.removed,
            "bytes": report.bytes,
            "kept_alive": report.kept_alive,
            "over_cap": report.over_cap,
        }),
    )?;
    Ok(report)
}

/// Delete one blob, counting it.
fn remove(blob: &Blob, report: &mut Pruned) -> io::Result<()> {
    fs::remove_file(&blob.path)?;
    report.removed += 1;
    report.bytes += blob.bytes;
    Ok(())
}

/// Every blob under the store, ignoring anything that is not one.
///
/// Only `<root>/<kind>/<64 hex>` counts. This is the one part of the process that
/// deletes, so it deletes what it recognises rather than what it finds: something
/// else that landed in the directory is left where it is.
fn walk(root: &Path) -> io::Result<Vec<Blob>> {
    let mut blobs = Vec::new();
    for kind in fs::read_dir(root)? {
        let kind = kind?;
        if !kind.file_type()?.is_dir() {
            continue;
        }
        for entry in fs::read_dir(kind.path())? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if !metadata.is_file() {
                continue;
            }
            let Some(digest) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !is_digest_name(&digest) {
                continue;
            }
            blobs.push(Blob {
                path: entry.path(),
                digest,
                bytes: metadata.len(),
                written: metadata.modified()?,
            });
        }
    }
    Ok(blobs)
}

/// The digests named by every journal line at or after `cutoff`.
///
/// A line whose timestamp cannot be read counts as inside the window: the cost of
/// keeping bytes too long is disk, and the cost of removing bytes a turn still
/// names is the evidence itself.
fn named_since(journal: &Journal, cutoff: SystemTime) -> io::Result<BTreeSet<String>> {
    let text = match fs::read_to_string(journal.path()) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(error),
    };
    let mut digests = BTreeSet::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        // A line that is not a journal entry cannot be dated and cannot be read for
        // the names it holds; it is skipped rather than guessed at.
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let recent = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(stamp)
            .is_none_or(|at| at >= cutoff);
        if recent {
            digests.extend(crate::digests_in(&entry));
        }
    }
    Ok(digests)
}

/// The instant a journal timestamp names.
fn stamp(text: &str) -> Option<SystemTime> {
    OffsetDateTime::parse(text, &Rfc3339).ok().map(SystemTime::from)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::{ArchiveRef, KIND_REQUEST};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    /// A store and a journal of its own, and nothing else: every test below states
    /// the age of what it stored rather than waiting for the clock.
    fn fixture(label: &str) -> (ArchiveStore, Journal, PathBuf) {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("bifrost-retention-{}-{label}-{unique}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        (
            ArchiveStore::new(root.join("archive")),
            Journal::new(root.join("journal.jsonl")),
            root,
        )
    }

    /// Store `body`, then say it was written `age` ago.
    fn stored(store: &ArchiveStore, body: &[u8], age: Duration) -> String {
        let reference = store.put(KIND_REQUEST, body).expect("store");
        let path = store.path_for(KIND_REQUEST, &reference);
        fs::File::options()
            .write(true)
            .open(&path)
            .expect("open")
            .set_modified(SystemTime::now() - age)
            .expect("set mtime");
        reference.sha256
    }

    /// What is left in the store.
    fn remaining(store: &ArchiveStore) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(store.root().join(KIND_REQUEST))
            .expect("list")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort_unstable();
        names
    }

    fn age_of(days: u64) -> Retention {
        Retention {
            max_age: Some(Duration::from_secs(days * 24 * 60 * 60)),
            max_bytes: None,
        }
    }

    #[test]
    fn a_blob_past_the_age_is_removed_and_the_pass_is_recorded() {
        let (store, journal, _root) = fixture("expired");
        let body = b"expired bytes";
        stored(&store, body, 3 * DAY);

        let report = prune(&store, &journal, &age_of(1), SystemTime::now()).expect("prune");

        assert_eq!(report.removed, 1);
        assert_eq!(report.bytes, body.len() as u64);
        assert_eq!(report.kept_alive, 0);
        assert!(!report.over_cap);
        assert!(remaining(&store).is_empty(), "the bytes are gone");

        let entries = journal.read_all().expect("read journal");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["kind"], crate::KIND_RETENTION);
        assert_eq!(entries[0]["data"]["removed"], 1);
        assert_eq!(entries[0]["data"]["bytes"], body.len());
    }

    #[test]
    fn a_blob_inside_the_age_is_left_alone() {
        let (store, journal, _root) = fixture("recent");
        let body = b"yesterday's answer";
        let digest = stored(&store, body, DAY / 24);

        let report = prune(&store, &journal, &age_of(1), SystemTime::now()).expect("prune");

        assert_eq!(report.removed, 0, "nothing inside the window is a candidate");
        assert_eq!(remaining(&store), vec![digest.clone()]);
        let reference = ArchiveRef {
            sha256: digest,
            bytes: body.len() as u64,
            lines: 1,
        };
        assert_eq!(store.read(KIND_REQUEST, &reference).expect("read"), body);
    }

    /// The case the reference guard exists for: identical bytes are stored once, so
    /// the file an old turn created can be the file a turn from today was made of.
    #[test]
    fn a_blob_a_recent_line_names_survives_its_age() {
        let (store, journal, _root) = fixture("named");
        let digest = stored(&store, b"the same question, again", 3 * DAY);
        journal
            .append("turn", serde_json::json!({ "request": { "sha256": digest } }))
            .expect("journal");

        let report = prune(&store, &journal, &age_of(1), SystemTime::now()).expect("prune");

        assert_eq!(report.removed, 0);
        assert_eq!(report.kept_alive, 1, "the file is old, the turn that names it is not");
        assert_eq!(remaining(&store), vec![digest]);
    }

    /// A line from before the window is not a reason to keep anything: if it were,
    /// the journal — which is never pruned — would make retention a no-op.
    #[test]
    fn a_blob_only_an_old_line_names_is_removed() {
        let (store, journal, _root) = fixture("old-line");
        let digest = stored(&store, b"already answered", 400 * DAY);
        fs::write(
            journal.path(),
            format!(
                "{{\"timestamp\":\"2020-01-01T00:00:00Z\",\"kind\":\"turn\",\"data\":{{\"request\":{{\"sha256\":\"{digest}\"}}}}}}\n"
            ),
        )
        .expect("write journal");

        let report = prune(&store, &journal, &age_of(1), SystemTime::now()).expect("prune");

        assert_eq!(report.removed, 1);
        assert_eq!(report.kept_alive, 0);
        assert!(remaining(&store).is_empty());
        assert!(
            journal.path().metadata().expect("journal").len() > 0,
            "the journal itself is never pruned"
        );
    }

    /// A line that cannot be dated is kept, not guessed at: the cost of keeping
    /// bytes too long is disk, and the cost of the other mistake is the evidence.
    #[test]
    fn a_line_that_cannot_be_dated_protects_what_it_names() {
        let (store, journal, _root) = fixture("undated");
        let digest = stored(&store, b"undated", 3 * DAY);
        fs::write(
            journal.path(),
            format!("{{\"timestamp\":\"whenever\",\"kind\":\"turn\",\"data\":{{\"request\":{{\"sha256\":\"{digest}\"}}}}}}\n"),
        )
        .expect("write journal");

        let report = prune(&store, &journal, &age_of(1), SystemTime::now()).expect("prune");

        assert_eq!(report.removed, 0);
        assert_eq!(report.kept_alive, 1);
    }

    #[test]
    fn the_ceiling_takes_the_oldest_first() {
        let (store, journal, _root) = fixture("ceiling");
        let oldest = stored(&store, b"aaaaaaaaaa", 3 * DAY);
        let middle = stored(&store, b"bbbbbbbbbb", 2 * DAY);
        let newest = stored(&store, b"cccccccccc", DAY);

        let retention = Retention {
            max_age: None,
            max_bytes: Some(25),
        };
        let report = prune(&store, &journal, &retention, SystemTime::now()).expect("prune");

        assert_eq!(report.removed, 1);
        assert_eq!(report.bytes, 10);
        assert!(!report.over_cap);
        assert_eq!(remaining(&store), {
            let mut left = vec![middle, newest];
            left.sort_unstable();
            left
        });
        assert!(!store.root().join(KIND_REQUEST).join(oldest).exists());
    }

    /// A ceiling that cannot be met without breaking the window is reported rather
    /// than met: what the operator asked to keep outranks what they asked to save.
    #[test]
    fn a_ceiling_that_cannot_be_met_is_reported() {
        let (store, journal, _root) = fixture("over-cap");
        let first = stored(&store, b"aaaaaaaaaa", 3 * DAY);
        let second = stored(&store, b"bbbbbbbbbb", 2 * DAY);
        for digest in [&first, &second] {
            journal
                .append("turn", serde_json::json!({ "request": { "sha256": digest } }))
                .expect("journal");
        }

        let retention = Retention {
            max_age: Some(DAY),
            max_bytes: Some(5),
        };
        let report = prune(&store, &journal, &retention, SystemTime::now()).expect("prune");

        assert_eq!(report.removed, 0);
        assert_eq!(report.kept_alive, 2);
        assert!(report.over_cap, "the ceiling is exceeded and the pass says so");
        assert_eq!(remaining(&store).len(), 2);
    }

    /// A pass that found nothing is recorded like any other: a deployment within its
    /// retention and one whose retention nobody wired up look the same in the
    /// archive, and the journal is where the difference is.
    #[test]
    fn a_pass_that_found_no_store_still_records_itself() {
        let (store, journal, _root) = fixture("missing");

        let report = prune(&store, &journal, &age_of(1), SystemTime::now()).expect("prune");

        assert_eq!(report, Pruned::default());
        let entries = journal.read_all().expect("read journal");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["kind"], crate::KIND_RETENTION);
        assert_eq!(entries[0]["data"]["removed"], 0);
    }

    /// This is the only code in the crate that deletes, so it deletes the shape it
    /// writes and nothing else.
    #[test]
    fn a_file_that_is_not_a_digest_is_left_where_it_is() {
        let (store, journal, _root) = fixture("stray");
        stored(&store, b"real blob", 3 * DAY);
        let stray = store.root().join(KIND_REQUEST).join("README");
        fs::create_dir_all(store.root().join(KIND_REQUEST)).expect("kind dir");
        fs::write(&stray, b"not mine").expect("write stray");

        let report = prune(&store, &journal, &age_of(1), SystemTime::now()).expect("prune");

        assert_eq!(report.removed, 1, "the blob went");
        assert!(stray.exists(), "the file this crate did not write stayed");
    }
}
