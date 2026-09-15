//! Pruning the archive on the deployment's schedule.
//!
//! The archive is the one mechanism that writes a file per turn, so it is the one
//! that can fill a disk. What to keep is the deployment's policy and the archive
//! crate knows how to carry it out, so this module is the part in between: it reads
//! the policy from the configuration, decides whether there is anything to do, and
//! puts the pass on a schedule.
//!
//! A pass runs at startup and then once a day. The startup pass is what makes a
//! deployment that is restarted often prune at least once; the daily one is what
//! makes a deployment that runs for months prune at all.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bifrost_audit::{Pruned, Retention};

use crate::log::{info, warn};
use crate::state::Edge;

/// How often the archive is pruned after the first pass.
const EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// What this deployment asked to keep.
///
/// The two settings are independent: a window with no ceiling is "keep a month,
/// however large that turns out to be", and a ceiling with no window is "keep
/// whatever fits, oldest first".
#[must_use]
pub fn asked(edge: &Edge) -> Retention {
    Retention {
        max_age: edge.config().audit.retain_age(),
        max_bytes: edge.config().audit.max_total_bytes(),
    }
}

/// Prune once, saying what happened.
///
/// `Ok(None)` is a deployment that asked for nothing, or one whose archive is off —
/// the two are the same absence here, and neither is worth a line. `Err` is a pass
/// that could not finish, which is the operator's business: it means the disk keeps
/// filling.
pub fn prune_now(edge: &Edge) -> std::io::Result<Option<Pruned>> {
    let retention = asked(edge);
    if retention.is_off() {
        return Ok(None);
    }
    let Some(evidence) = edge.evidence() else {
        return Ok(None);
    };
    let report = evidence.prune(&retention, SystemTime::now())?;
    say(&report);
    Ok(Some(report))
}

/// One line per pass, in an operator's terms.
fn say(report: &Pruned) {
    info(format!(
        "pruned the archive: removed {} blob(s), {} byte(s); {} kept alive by a turn inside the retention window",
        report.removed, report.bytes, report.kept_alive
    ));
    if report.over_cap {
        // Everything left is evidence the deployment asked to keep, so the only two
        // ways out are the operator's: a smaller window or a larger ceiling.
        warn(
            "the archive is over audit.max_total_mb and everything left is named by a turn inside the window; \
             shorten audit.retain_days or raise the ceiling",
        );
    }
}

/// Start the pass at startup and every day after, if this deployment asked for one.
///
/// Started from the binary rather than from `app`, for the reason the drift watch
/// is: it is a property of the process, and a test that builds a router should not
/// begin deleting files.
#[must_use]
pub fn watch(edge: &Arc<Edge>) -> Option<tokio::task::JoinHandle<()>> {
    if asked(edge).is_off() || edge.evidence().is_none() {
        return None;
    }
    let edge = Arc::clone(edge);
    Some(tokio::spawn(async move {
        loop {
            // Off the runtime threads, as the archive's own writes are: a pass walks
            // a directory and reads a journal, and neither is a reason for a turn to
            // wait behind it.
            let edge = Arc::clone(&edge);
            match tokio::task::spawn_blocking(move || prune_now(&edge)).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => warn(format!("could not prune the archive: {error}")),
                Err(error) => warn(format!("the archive prune did not finish: {error}")),
            }
            tokio::time::sleep(EVERY).await;
        }
    }))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use bifrost_audit::KIND_REQUEST;
    use bifrost_config::{AuditConfig, Config, MechanismsConfig};

    use crate::evidence::TurnContext;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    /// A deployment of its own, archiving or not, keeping for `retain_days`.
    fn deployment(label: &str, retain_days: u32, archive: bool) -> (Arc<Edge>, PathBuf) {
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "bifrost-edge-retention-{}-{label}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let config = Config {
            mechanisms: MechanismsConfig {
                evidence_archive: archive,
                ..Default::default()
            },
            audit: AuditConfig {
                journal_dir: root.join("journal"),
                archive_dir: root.join("archive"),
                retain_days,
                max_total_mb: 0,
            },
            ..Config::default()
        };
        (Edge::new(config).expect("edge builds"), root)
    }

    /// Say a file was written `age` ago.
    fn backdate(path: &Path, age: Duration) {
        fs::File::options()
            .write(true)
            .open(path)
            .expect("open")
            .set_modified(SystemTime::now() - age)
            .expect("set mtime");
    }

    /// Archive a request and say it was archived `age` ago.
    fn archived(edge: &Edge, body: &[u8], age: Duration) -> PathBuf {
        let evidence = edge.evidence().expect("the archive is on");
        let captured = evidence.archive_request(body).expect("archive");
        let path = evidence
            .archive_dir()
            .join(KIND_REQUEST)
            .join(&captured.reference.sha256);
        backdate(&path, age);
        path
    }

    #[test]
    fn a_deployment_that_asked_for_nothing_prunes_nothing() {
        let (edge, _root) = deployment("off", 0, true);

        assert!(asked(&edge).is_off());
        assert!(prune_now(&edge).expect("nothing to report").is_none());
        assert!(watch(&edge).is_none(), "no policy, no task");
    }

    /// A window with no archive to apply it to is the same absence as no window.
    #[test]
    fn an_archive_that_is_off_is_not_pruned() {
        let (edge, _root) = deployment("no-archive", 7, false);

        assert!(!asked(&edge).is_off(), "the deployment asked to keep for a week");
        assert!(
            prune_now(&edge).expect("nothing to report").is_none(),
            "but there is no archive, so there is nothing to prune"
        );
        assert!(watch(&edge).is_none());
    }

    #[test]
    fn an_expired_blob_is_removed_and_the_pass_is_recorded() {
        let (edge, _root) = deployment("expired", 1, true);
        let path = archived(&edge, b"a question from last week", 8 * DAY);

        let report = prune_now(&edge).expect("prune").expect("a pass ran");

        assert_eq!(report.removed, 1);
        assert_eq!(report.bytes, b"a question from last week".len() as u64);
        assert!(!path.exists(), "the bytes are gone");

        let evidence = edge.evidence().expect("archive");
        let entries = bifrost_audit::Journal::new(evidence.journal_path())
            .read_all()
            .expect("journal");
        let last = entries.last().expect("a line was written");
        assert_eq!(last["kind"], "retention");
        assert_eq!(last["data"]["removed"], 1);
    }

    /// The wiring that matters: the pass is handed the journal, so a blob a recent
    /// turn names survives the age of the file it was stored in.
    #[test]
    fn a_blob_a_recent_turn_names_survives_the_pass() {
        let (edge, _root) = deployment("named", 1, true);
        let evidence = edge.evidence().expect("the archive is on");
        let captured = evidence.archive_request(b"the same question, again").expect("archive");
        let path = evidence
            .archive_dir()
            .join(KIND_REQUEST)
            .join(&captured.reference.sha256);
        evidence
            .record(
                &TurnContext {
                    protocol: "openai-chat".to_owned(),
                    model: "m".to_owned(),
                    session: "s".to_owned(),
                    key_fingerprint: "0123456789abcdef".to_owned(),
                }
                .turn(200, false, Some(captured), None),
            )
            .expect("journal");
        backdate(&path, 8 * DAY);

        let report = prune_now(&edge).expect("prune").expect("a pass ran");

        assert_eq!(report.removed, 0);
        assert_eq!(report.kept_alive, 1);
        assert!(path.exists(), "a turn inside the window still names these bytes");
    }

    /// A deployment that asked for a window gets a task, and the task's first pass
    /// has already happened by the time it is aborted: `watch` prunes before it
    /// sleeps, so a process that just came up is not waiting a day to be within its
    /// retention.
    #[tokio::test]
    async fn a_deployment_with_a_window_starts_a_task_that_prunes_at_once() {
        let (edge, _root) = deployment("scheduled", 1, true);
        let path = archived(&edge, b"an old question", 8 * DAY);

        let handle = watch(&edge).expect("a task");
        for _ in 0..100 {
            if !path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        handle.abort();

        assert!(!path.exists(), "the startup pass ran before the first sleep");
    }
}
