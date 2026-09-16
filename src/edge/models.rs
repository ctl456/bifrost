//! The catalogue `/v1/models` answers with.
//!
//! The list can come from two places. The upstream publishes one, at the
//! dialect's own catalogue path, and it is the better answer when it can be had:
//! it is what the provider offers, which is more than the compiled-in table knows
//! about. Failing that there is a table compiled into this build, which is what
//! the deployment was tested against.
//!
//! What it is not is a statement about this account's plan: the upstream lists
//! models a plan does not include, and those answer `401 MODEL_NOT_IN_PLAN` when
//! they are asked for. A Go plan in this repository's own checks lists
//! `claude-sonnet-5`, `gpt-5.5` and `google/gemini-3.8-flash` and is refused all
//! three. The only answer to "may I use this" is asking for it.
//!
//! Where the original falls back to that table whenever a refresh fails, this
//! serves the last list the upstream gave it instead. A catalogue the upstream
//! actually produced is evidence of what it serves; one compiled in at build time
//! is a memory. The built-in table is for a deployment that has never had an
//! answer, not for one that has stopped hearing the newest one.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::core::Error;
use crate::wire::WireRequest;
use serde_json::Value;

use crate::edge::log::warn;
use crate::edge::state::Edge;
use crate::edge::upstream;

/// What is served when the upstream cannot be asked.
///
/// The original carries a display name beside each id and then never sends it, so
/// there are no names here: the only thing `/v1/models` puts on the wire is the id.
pub const BUILT_IN: &[&str] = &[
    "claude-sonnet-4-6",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-haiku-4-5-20251001",
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.3-codex",
    "deepseek/deepseek-v4-pro",
    "deepseek/deepseek-v4-flash",
    "moonshotai/Kimi-K2.6",
    "moonshotai/Kimi-K2.5",
    "zai-org/GLM-5.1",
    "zai-org/GLM-5",
    "MiniMaxAI/MiniMax-M3",
    "MiniMaxAI/MiniMax-M2.7",
    "MiniMaxAI/MiniMax-M2.5",
    "Qwen/Qwen3.6-Max-Preview",
    "Qwen/Qwen3.6-Plus",
    "Qwen/Qwen3.7-Max",
    "stepfun/Step-3.7-Flash",
    "stepfun/Step-3.5-Flash",
    "xiaomi/mimo-v2.5-pro",
    "xiaomi/mimo-v2.5",
    "google/gemini-3.5-flash",
    "google/gemini-3.1-flash-lite",
];

/// A list the upstream gave, and when it gave it.
#[derive(Debug)]
struct Fetched {
    ids: Vec<String>,
    at: Instant,
}

/// The cached catalogue, and the gate that keeps one fetch from becoming many.
///
/// Shared by the whole deployment rather than kept per key, as the original shares
/// it: a catalogue is fetched once every few minutes from an endpoint clients call
/// when they start, and a map of key to list would be one more thing that grows
/// with the number of keys.
#[derive(Debug)]
pub struct Catalog {
    fetched: Mutex<Option<Fetched>>,
    fetching: tokio::sync::Mutex<()>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self::new()
    }
}

impl Catalog {
    #[must_use]
    pub fn new() -> Self {
        Self {
            fetched: Mutex::new(None),
            fetching: tokio::sync::Mutex::new(()),
        }
    }

    /// The list as it stands, and whether it is still inside its lifetime.
    fn cached(&self, ttl: Duration) -> Option<(Vec<String>, bool)> {
        self.fetched
            .lock()
            .expect("catalogue poisoned")
            .as_ref()
            .map(|fetched| (fetched.ids.clone(), fetched.at.elapsed() < ttl))
    }

    fn remember(&self, ids: Vec<String>) {
        *self.fetched.lock().expect("catalogue poisoned") = Some(Fetched {
            ids,
            at: Instant::now(),
        });
    }
}

/// The model ids to offer a client.
///
/// A missing key is not an error: the built-in table needs nothing, and a caller
/// that has not identified itself has not earned a call to the upstream's own
/// catalogue either.
pub async fn catalog(edge: &Edge, api_key: Option<&str>) -> Vec<String> {
    let config = &edge.config().models;
    if !config.provider {
        return built_in();
    }
    if let Some((ids, true)) = edge.catalog().cached(config.refresh()) {
        return ids;
    }

    let Some(request) = api_key.and_then(|key| edge.wire().provider_models(key)) else {
        return built_in();
    };

    // One fetch at a time. A fleet of clients starting together would otherwise
    // each ask, and they would all be asking the same question.
    let _fetching = edge.catalog().fetching.lock().await;
    if let Some((ids, true)) = edge.catalog().cached(config.refresh()) {
        return ids;
    }

    match fetch(edge, &request).await {
        Some(ids) => {
            edge.catalog().remember(ids.clone());
            ids
        }
        // The last list the upstream gave, if it ever gave one.
        None => match edge.catalog().cached(Duration::MAX) {
            Some((ids, _)) => ids,
            None => built_in(),
        },
    }
}

/// The built-in table, as an owned list.
fn built_in() -> Vec<String> {
    BUILT_IN.iter().map(|id| (*id).to_owned()).collect()
}

/// Ask the upstream what it offers.
///
/// Not what this account is entitled to: the answer includes models the plan does
/// not cover, which is why nothing here is ever picked automatically.
///
/// Every way of not getting an answer — a refused status, an unreadable body, a
/// body that is not a catalogue, a fetch that never returns — ends in `None`, and
/// the caller decides what to serve instead.
async fn fetch(edge: &Edge, request: &WireRequest) -> Option<Vec<String>> {
    let limit = edge.config().models.timeout();
    let attempt = async {
        let upstream = upstream::send(edge, request).await?;
        let status = upstream.status;
        let text = upstream.text().await;
        Ok::<(u16, String), Error>((status, text))
    };

    let (status, text) = match tokio::time::timeout(limit, attempt).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(error)) => {
            warn(format!("could not fetch the model catalogue: {error}"));
            return None;
        }
        Err(_elapsed) => {
            warn(format!(
                "the model catalogue did not answer within {}ms",
                limit.as_millis()
            ));
            return None;
        }
    };

    if !(200..300).contains(&status) {
        warn(format!("the model catalogue was refused with status {status}"));
        return None;
    }
    match parse(&text) {
        Some(ids) => Some(ids),
        None => {
            warn("the model catalogue was not a list of models");
            None
        }
    }
}

/// Read the ids out of a catalogue body.
///
/// An empty list is an answer rather than a failure: the upstream offered
/// nothing, and second-guessing that with a built-in table would offer models the
/// upstream does not serve. Only a body that is not a catalogue at all is `None`.
#[must_use]
pub fn parse(body: &str) -> Option<Vec<String>> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    let data = parsed.get("data")?.as_array()?;
    Some(
        data.iter()
            .filter_map(|model| Some(model.get("id")?.as_str()?.to_owned()))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_catalogue_is_its_ids_in_order() {
        let ids = parse(r#"{"object":"list","data":[{"id":"a"},{"id":"b"},{"id":"c"}]}"#).expect("a catalogue");
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn an_empty_catalogue_is_an_answer() {
        assert_eq!(parse(r#"{"data":[]}"#), Some(Vec::new()));
    }

    #[test]
    fn a_body_that_is_not_a_catalogue_is_not_an_answer() {
        assert_eq!(parse("<html>gateway timeout</html>"), None);
        assert_eq!(parse(r#"{"data":"nope"}"#), None);
        assert_eq!(parse(r#"{"models":[]}"#), None);
    }

    /// An entry without an id is skipped rather than made into an empty id: a
    /// client asking for `""` would be asking for nothing in particular.
    #[test]
    fn entries_without_an_id_are_skipped() {
        assert_eq!(
            parse(r#"{"data":[{"id":"a"},{"name":"b"},{"id":7}]}"#),
            Some(vec!["a".to_owned()])
        );
    }

    #[test]
    fn the_built_in_table_is_owned_per_call() {
        let first = built_in();
        assert_eq!(first.len(), BUILT_IN.len());
        assert_eq!(first[0], "claude-sonnet-4-6");
    }
}
