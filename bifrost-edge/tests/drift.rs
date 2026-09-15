//! The version drift watch, against a registry that is not npm.
//!
//! The check reads a document from whatever `wire.drift_registry` names and
//! compares the version in it with the one this build speaks. Pointing it at a
//! stub is what makes that testable at all: what npm happens to be serving is not
//! part of a test run, and the comparison is the interesting half.

use std::sync::{Arc, Mutex};

use bifrost_config::{Config, WireConfig};
use bifrost_edge::Edge;
use bifrost_edge::drift::{self, Verdict};

/// A registry stub, and the paths it was asked for.
struct Registry {
    address: String,
    paths: Arc<Mutex<Vec<String>>>,
}

impl Registry {
    /// The last path read, which panics if there was none.
    fn last_path(&self) -> String {
        self.paths
            .lock()
            .expect("record")
            .last()
            .cloned()
            .expect("the registry was read")
    }

    /// Every path read, in order.
    fn paths(&self) -> Vec<String> {
        self.paths.lock().expect("record").clone()
    }
}

/// A registry that answers every path with `status` and `body`.
async fn registry(status: u16, body: &'static str) -> Registry {
    let paths = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&paths);

    let router = axum::Router::new().fallback(move |request: axum::extract::Request| {
        let recorder = Arc::clone(&recorder);
        async move {
            recorder.lock().expect("record").push(request.uri().path().to_owned());
            (
                axum::http::StatusCode::from_u16(status).expect("status"),
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                body,
            )
        }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Registry {
        address: format!("http://{address}"),
        paths,
    }
}

/// An edge whose drift check reads from `registry`.
fn edge_pointing_at(registry: &str) -> Arc<Edge> {
    let config = Config {
        wire: WireConfig {
            drift_registry: registry.to_owned(),
            ..WireConfig::default()
        },
        ..Config::default()
    };
    Edge::new(config).expect("edge builds")
}

/// The package is read from the dialect and the path from the registry, so the
/// two together are what the check actually asks for.
#[tokio::test]
async fn a_published_version_ahead_of_this_build_is_drift() {
    let registry = registry(200, r#"{"name":"command-code","version":"1.54.0"}"#).await;
    let edge = edge_pointing_at(&registry.address);

    let report = drift::check(&edge).await.expect("the registry answered");
    assert_eq!(report.published, "1.54.0");
    assert_eq!(report.verdict, Verdict::Behind);
    assert_eq!(registry.last_path(), "/command-code/latest");
}

#[tokio::test]
async fn the_published_version_of_this_build_is_in_sync() {
    let registry = registry(200, r#"{"version":"1.53.1"}"#).await;
    let edge = edge_pointing_at(&registry.address);

    let report = drift::check(&edge).await.expect("the registry answered");
    assert_eq!(report.verdict, Verdict::InSync);
}

/// A registry behind a path prefix is a mirror, which is the reason the setting
/// is a base rather than a bare host.
#[tokio::test]
async fn a_mirror_keeps_its_prefix() {
    let registry = registry(200, r#"{"version":"1.53.1"}"#).await;
    let edge = edge_pointing_at(&format!("{}/npm/", registry.address));

    assert!(drift::check(&edge).await.is_some());
    assert_eq!(registry.last_path(), "/npm/command-code/latest");
}

/// A pre-release of a later version is not a release this build should have
/// implemented, and it is not reported as if it were.
#[tokio::test]
async fn a_version_that_does_not_order_is_reported_as_such() {
    let registry = registry(200, r#"{"version":"1.54.0-rc.1"}"#).await;
    let edge = edge_pointing_at(&registry.address);

    let report = drift::check(&edge).await.expect("the registry answered");
    assert_eq!(report.published, "1.54.0-rc.1");
    assert_eq!(report.verdict, Verdict::Different);
}

/// A registry that refuses has not said anything, and a refusal is not agreement.
#[tokio::test]
async fn a_registry_that_refuses_is_not_an_answer() {
    let registry = registry(503, "{}").await;
    let edge = edge_pointing_at(&registry.address);

    assert_eq!(drift::check(&edge).await, None);
}

#[tokio::test]
async fn a_body_that_is_not_a_registry_document_is_not_an_answer() {
    let registry = registry(200, "<html>gateway timeout</html>").await;
    let edge = edge_pointing_at(&registry.address);

    assert_eq!(drift::check(&edge).await, None);
}

/// The switch is the whole of the loop: off means no task, and so no read.
#[tokio::test]
async fn a_deployment_that_did_not_ask_does_not_watch() {
    let registry = registry(200, r#"{"version":"1.54.0"}"#).await;
    let config = Config {
        wire: WireConfig {
            drift_watch: false,
            drift_registry: registry.address.clone(),
            ..WireConfig::default()
        },
        ..Config::default()
    };
    let edge = Edge::new(config).expect("edge builds");

    assert!(drift::watch(&edge).is_none());
    assert!(registry.paths().is_empty(), "nothing should have been read");
}

/// A deployment that asked watches whether or not anyone is listening: the check
/// is the first thing the task does.
#[tokio::test]
async fn a_deployment_that_asked_watches() {
    let registry = registry(200, r#"{"version":"1.53.1"}"#).await;
    let edge = edge_pointing_at(&registry.address);

    let watch = drift::watch(&edge).expect("the watch was asked for");
    // The check runs before the first interval, so one path is read without
    // waiting a day for it.
    for _ in 0..100 {
        if !registry.paths().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(registry.last_path(), "/command-code/latest");
    watch.abort();
}
