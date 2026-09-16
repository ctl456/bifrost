//! The version drift watch.
//!
//! The version this build reports upstream is the one it implements, and it never
//! moves on its own: claiming a version whose shape is not implemented is a
//! stronger signal to a behavioral detector than claiming an older one. So the
//! upstream's client moving ahead is not something to follow — it is something to
//! be told about, because the dialect was read off that client and may have moved
//! with it.
//!
//! Nothing here touches a request. A check reads the published version, compares
//! it with the implemented one, and says so; the answer it cannot bring itself to
//! trust is reported as unreadable rather than as agreement.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;

use crate::edge::log::{info, warn};
use crate::edge::state::Edge;

/// How often the published version is read.
///
/// A day, as in the original: this is a reminder to re-read a package, not a
/// signal to act on, and a published version does not move twice a day.
const REFRESH: Duration = Duration::from_secs(24 * 60 * 60);

/// How long one read may take.
const TIMEOUT: Duration = Duration::from_secs(10);

/// How much of a registry document is read before it is called a problem.
///
/// A registry document is a few kilobytes. The ceiling is here because the host
/// is named by configuration, and a misdirected one should not be able to make
/// this process hold an unbounded buffer.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// What a published version means for this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// And they are the same version.
    InSync,
    /// The client has moved ahead: the dialect may have moved with it.
    Behind,
    /// This build is ahead of what is published.
    Ahead,
    /// The two cannot be ordered, so they are not claimed to be one thing.
    Different,
}

/// What one check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The version the registry currently serves.
    pub published: String,
    /// What that version means here.
    pub verdict: Verdict,
}

/// Order two `major.minor.patch` versions, when both are versions.
///
/// Build metadata does not order — `1.53.1+build.5` is that release. A
/// pre-release does order, and is not something to re-align to, so it is left
/// unparsed rather than compared as a release it is not yet.
fn order(version: &str) -> Option<(u64, u64, u64)> {
    let release = version.split('+').next().unwrap_or(version);
    if release.contains('-') {
        return None;
    }
    let release = release.strip_prefix('v').unwrap_or(release);
    let mut parts = release.split('.').map(str::parse::<u64>);
    let triple = (parts.next()?.ok()?, parts.next()?.ok()?, parts.next()?.ok()?);
    parts.next().is_none().then_some(triple)
}

/// What the published version means for this build.
#[must_use]
pub fn compare(published: &str, implemented: &str) -> Verdict {
    if published == implemented {
        return Verdict::InSync;
    }
    match (order(published), order(implemented)) {
        // Build metadata is not part of precedence, so `1.53.1+build.5` is the
        // release this build implements rather than a version after it.
        (Some(published), Some(implemented)) if published == implemented => Verdict::InSync,
        (Some(published), Some(implemented)) if published > implemented => Verdict::Behind,
        (Some(_), Some(_)) => Verdict::Ahead,
        _ => Verdict::Different,
    }
}

/// The version out of a registry document.
fn published_version(body: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    Some(parsed.get("version")?.as_str()?.to_owned())
}

/// Read what the dialect's client is published as, and what that means.
///
/// `None` covers every way of not getting an answer — no published reference, a
/// refused status, a body that is not a document, a read that overran its ceiling
/// or its deadline. The caller says so; none of it is a reason to change a
/// request.
pub async fn check(edge: &Edge) -> Option<Report> {
    let package = edge.wire().published_package()?;
    let registry = edge.config().wire.drift_registry.trim_end_matches('/');
    let url = format!("{registry}/{package}/latest");

    let read = async {
        let response = edge.client().get(&url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        published_version(&read_body(response).await?)
    };

    let published = tokio::time::timeout(TIMEOUT, read).await.ok().flatten()?;
    let verdict = compare(&published, edge.wire().version());
    Some(Report { published, verdict })
}

/// Read a body, refusing one that will not fit.
async fn read_body(response: reqwest::Response) -> Option<String> {
    let mut stream = response.bytes_stream();
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).ok()
}

/// Start the watch, if this deployment asked for it and the dialect names
/// something published.
///
/// The check runs at startup rather than after the first interval, so a process
/// that has just come up knows where it stands. Nothing here is on a request
/// path: the task reads a registry and writes a line, and a registry that never
/// answers costs a warning and nothing else. A process that outlives the interval
/// checks again; the deployment is expected to have been re-deployed by then.
#[must_use]
pub fn watch(edge: &Arc<Edge>) -> Option<tokio::task::JoinHandle<()>> {
    if !edge.config().wire.drift_watch {
        return None;
    }
    let package = edge.wire().published_package()?;
    let edge = Arc::clone(edge);

    Some(tokio::spawn(async move {
        loop {
            match check(&edge).await {
                Some(report) => say(&report, edge.wire().version()),
                None => warn(format!("could not read the published version of {package}")),
            }
            tokio::time::sleep(REFRESH).await;
        }
    }))
}

/// Say what one check found, in an operator's terms.
fn say(report: &Report, implemented: &str) {
    match report.verdict {
        Verdict::InSync => info(format!(
            "the client is published at {implemented}, which is what this build speaks"
        )),
        Verdict::Behind => warn(format!(
            "version drift: the client is published at {} while this build speaks {implemented}; re-read the package and re-align the dialect",
            report.published
        )),
        Verdict::Ahead => info(format!(
            "this build speaks {implemented}, ahead of the published {}",
            report.published
        )),
        Verdict::Different => warn(format!(
            "the published version {} does not order against the implemented {implemented}",
            report.published
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_implemented_version_is_in_sync_with_itself() {
        assert_eq!(compare("1.53.1", "1.53.1"), Verdict::InSync);
    }

    /// The case the watch exists for: the client moved and the dialect may have.
    #[test]
    fn a_newer_published_version_is_drift() {
        assert_eq!(compare("1.53.2", "1.53.1"), Verdict::Behind);
        assert_eq!(compare("1.54.0", "1.53.1"), Verdict::Behind);
        assert_eq!(compare("2.0.0", "1.53.1"), Verdict::Behind);
    }

    /// A build ahead of the registry is not a deployment that needs a warning
    /// about being behind.
    #[test]
    fn an_older_published_version_is_not_drift() {
        assert_eq!(compare("1.53.0", "1.53.1"), Verdict::Ahead);
        assert_eq!(compare("1.52.9", "1.53.1"), Verdict::Ahead);
    }

    /// Sorting versions as text would put `1.53.10` before `1.53.9`.
    #[test]
    fn each_part_is_a_number() {
        assert_eq!(compare("1.53.10", "1.53.9"), Verdict::Behind);
        assert_eq!(compare("1.9.0", "1.10.0"), Verdict::Ahead);
    }

    #[test]
    fn build_metadata_does_not_make_a_version_different() {
        assert_eq!(compare("1.53.1+build.5", "1.53.1"), Verdict::InSync);
    }

    /// A pre-release is not a release to re-align to, and `latest` is not a
    /// version at all; neither is guessed at.
    #[test]
    fn a_version_that_does_not_order_is_not_guessed_at() {
        assert_eq!(compare("1.54.0-rc.1", "1.53.1"), Verdict::Different);
        assert_eq!(compare("latest", "1.53.1"), Verdict::Different);
        assert_eq!(compare("1.53", "1.53.1"), Verdict::Different);
        assert_eq!(compare("1.53.1.0", "1.53.1"), Verdict::Different);
    }

    #[test]
    fn the_version_comes_out_of_a_registry_document() {
        let document = r#"{"name":"command-code","dist-tags":{"latest":"1.54.0"},"version":"1.54.0"}"#;
        assert_eq!(published_version(document).as_deref(), Some("1.54.0"));
        assert_eq!(published_version(r#"{"name":"command-code"}"#), None);
        assert_eq!(published_version(r#"{"version":153}"#), None);
        assert_eq!(published_version("<html>gateway timeout</html>"), None);
    }
}
