//! The edge end to end, against a real upstream.
//!
//! Nothing here is mocked: the edge binds a socket, the "upstream" is another
//! axum server on a loopback port, and the client is `reqwest`. That is the only
//! way to test the parts that are actually transport behavior — that a 413 is
//! produced before the whole body is buffered, that a stream is committed by its
//! first visible frame, that a zero-output turn still gets a retryable status.
//!
//! A unit test with a fake transport would assert the same logic against a
//! transport that does not exist, which is exactly where the interesting bugs
//! live.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use axum::body::Bytes;
use axum::response::IntoResponse;
use bifrost::config::{AccessConfig, Config, DeviceConfig, LimitsConfig, MechanismsConfig, ModelsConfig};
use bifrost::edge::access::Document;
use bifrost::edge::auth::key_fingerprint;
use bifrost::edge::models::BUILT_IN;
use bifrost::edge::{Edge, app};
use bifrost::fingerprint::{DeviceProfile, generate_fingerprint};
use serde_json::{Value, json};

/// One request the fake upstream was sent.
struct Request {
    path: String,
    body: Value,
    headers: Vec<(String, String)>,
}

impl Request {
    /// A header of this request, lowercased, if it carried one.
    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    }
}

/// Everything the fake upstream was asked.
///
/// Keyed by path rather than kept as "the last request", because the edge makes
/// more than one kind of call: a turn is a generate, and the first turn of a
/// window is a generate with an announcement in front of it. A recorder that
/// remembered one of them would make every test that reads it depend on the other
/// mechanisms being off.
#[derive(Default)]
struct Received {
    requests: Vec<Request>,
}

impl Received {
    /// Everything sent to `path`, in order.
    fn to(&self, path: &str) -> Vec<&Request> {
        self.requests.iter().filter(|request| request.path == path).collect()
    }

    /// The last request sent to `path`, which panics if there was none.
    fn last(&self, path: &str) -> &Request {
        self.to(path)
            .pop()
            .unwrap_or_else(|| panic!("nothing was sent to {path}; the upstream saw {}", self.seen()))
    }

    /// How many requests went to `path`.
    fn count(&self, path: &str) -> usize {
        self.to(path).len()
    }

    /// Every path seen, in order, for a failure message that says what happened.
    fn seen(&self) -> String {
        let paths: Vec<&str> = self.requests.iter().map(|request| request.path.as_str()).collect();
        paths.join(", ")
    }
}

/// Start a fake upstream that answers every path the same way.
async fn upstream<F>(answer: F) -> (String, Arc<Mutex<Received>>)
where
    F: Fn() -> (u16, String, &'static str) + Send + Sync + Clone + 'static,
{
    routed_upstream(move |_path| answer()).await
}

/// Start a fake upstream that answers each path with `answer`, and record what it
/// was sent.
async fn routed_upstream<F>(answer: F) -> (String, Arc<Mutex<Received>>)
where
    F: Fn(&str) -> (u16, String, &'static str) + Send + Sync + Clone + 'static,
{
    let received = Arc::new(Mutex::new(Received::default()));
    let recorder = Arc::clone(&received);

    let router = axum::Router::new().fallback(move |request: axum::extract::Request| {
        let answer = answer.clone();
        let recorder = Arc::clone(&recorder);
        async move {
            let (parts, body) = request.into_parts();
            let path = parts.uri.path().to_owned();
            let bytes = axum::body::to_bytes(body, 4 * 1024 * 1024).await.unwrap_or_default();
            let (status, body, content_type) = answer(&path);
            {
                let mut received = recorder.lock().expect("record");
                received.requests.push(Request {
                    path,
                    body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
                    headers: parts
                        .headers
                        .iter()
                        .map(|(name, value)| (name.as_str().to_owned(), value.to_str().unwrap_or_default().to_owned()))
                        .collect(),
                });
            }
            (
                axum::http::StatusCode::from_u16(status).expect("status"),
                [(axum::http::header::CONTENT_TYPE, content_type)],
                body,
            )
        }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{address}"), received)
}

/// Start the edge in front of `api_base` and return its address.
async fn edge(api_base: &str) -> String {
    let config = Config {
        api_base: api_base.to_owned(),
        ..Config::default()
    };
    serve(config).await
}

async fn serve(config: Config) -> String {
    let edge = Edge::new(config).expect("edge builds");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app(edge)).await;
    });
    format!("http://{address}")
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("client")
}

/// Read `/status`, which every count assertion starts from.
///
/// With the credential a turn would use where tokens are issued, and with none where
/// the key is forwarded: the body names callers exactly where there are tokens to
/// name, and a page that names callers is not one to hand to whatever can reach the
/// port. The argument the other shape made still holds where it applied — a count
/// you must authenticate to read is useless in the case it exists for, a deployment
/// whose credential is not working — so the deployments here that name nobody are
/// still read with nothing at all.
async fn status_of(edge: &str, credential: Option<&str>) -> Value {
    let response = status_from(edge, credential).await;
    assert_eq!(response.status(), 200, "the page answers whoever may read it");
    response.json().await.expect("json")
}

/// A read of `/status` that sends whatever credential it is given, answering the
/// response itself: the refusals are as much of the behaviour as the body is.
async fn status_from(edge: &str, credential: Option<&str>) -> reqwest::Response {
    let mut request = client().get(format!("{edge}/status"));
    if let Some(credential) = credential {
        request = request.header("authorization", format!("Bearer {credential}"));
    }
    request.send().await.expect("send")
}

fn parsed_tokens(body: &str) -> Value {
    serde_json::from_str::<Value>(body).expect("json")["tokens"].clone()
}

/// The path a turn is generated through.
const GENERATE: &str = "/alpha/generate";

/// The path the upstream publishes its own model catalogue at.
const CATALOGUE: &str = "/provider/v1/models";

/// The NDJSON a healthy upstream stream consists of.
const DELTAS: &str = concat!(
    "{\"type\":\"text-delta\",\"text\":\"Hel\"}\n",
    "{\"type\":\"text-delta\",\"text\":\"lo\"}\n",
    "{\"type\":\"finish\",\"finishReason\":\"stop\",\"totalUsage\":{\"inputTokens\":10,\"outputTokens\":3}}\n",
);

#[tokio::test]
async fn liveness_answers_and_is_cors_readable() {
    let (upstream, _) = upstream(|| (200, String::new(), "application/json")).await;
    let edge = edge(&upstream).await;

    let response = client().get(format!("{edge}/health")).send().await.expect("send");
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get("access-control-allow-origin").expect("cors"),
        "*"
    );
    assert_eq!(response.text().await.expect("body"), "OK");

    let response = client()
        .request(reqwest::Method::OPTIONS, format!("{edge}/v1/chat/completions"))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), 204, "a preflight is answered without a body");
}

#[tokio::test]
async fn an_unknown_path_is_a_not_found_in_the_openai_shape() {
    let (upstream, _) = upstream(|| (200, String::new(), "application/json")).await;
    let edge = edge(&upstream).await;

    let response = client().get(format!("{edge}/nope")).send().await.expect("send");
    assert_eq!(response.status(), 404);
    let body: Value = response.json().await.expect("json");
    assert_eq!(
        body,
        json!({ "error": { "message": "Not found", "type": "not_found" } })
    );
}

#[tokio::test]
async fn a_missing_key_is_reported_after_the_body_is_read() {
    let (upstream, _) = upstream(|| (200, String::new(), "application/json")).await;
    let edge = edge(&upstream).await;

    let response = client()
        .post(format!("{edge}/v1/messages"))
        .json(&json!({ "model": "m", "messages": [] }))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), 401);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["type"], json!("error"), "Anthropic wraps its errors");
    assert_eq!(body["error"]["type"], json!("authentication_error"));
}

#[tokio::test]
async fn a_malformed_body_is_reported_before_the_key_is_checked() {
    let (upstream, _) = upstream(|| (200, String::new(), "application/json")).await;
    let edge = edge(&upstream).await;

    // No key either, so the order of the two checks is what this pins: a
    // malformed request is a malformed request whether or not it is
    // authenticated, and reporting it as 401 sends the client to fix the wrong
    // thing.
    let response = client()
        .post(format!("{edge}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body("{ not json")
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn an_oversized_body_is_refused_with_413() {
    let (upstream, _) = upstream(|| (200, String::new(), "application/json")).await;
    let config = Config {
        api_base: upstream.clone(),
        limits: bifrost::config::LimitsConfig {
            max_body_mb: 1,
            ..Default::default()
        },
        ..Config::default()
    };
    let edge = serve(config).await;

    let huge = "x".repeat(2 * 1024 * 1024);
    let response = client()
        .post(format!("{edge}/v1/chat/completions"))
        .header("authorization", "Bearer user_abc123")
        .json(&json!({ "model": "m", "messages": [{ "role": "user", "content": huge }] }))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), 413);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["type"], json!("invalid_request_error"));
    assert!(body["error"]["message"].as_str().expect("message").contains("1MB"));
}

/// What the process counts, read back over HTTP.
///
/// The numbers exist to answer the question a deployment that is refusing turns
/// leaves open — is nothing arriving, or is nothing working — so the test that
/// matters is that a request refused for a reason is counted under that reason and
/// a request that worked is counted as a turn. Each one is driven through the real
/// route: a counter that is correct but never reached counts nothing, and no amount
/// of unit-testing the increment would catch that.
#[tokio::test]
async fn the_counts_name_the_reason_a_request_did_not_become_a_turn() {
    let (upstream, _) = upstream(|| (200, DELTAS.to_owned(), "application/x-ndjson")).await;
    let config = Config {
        api_base: upstream.clone(),
        limits: LimitsConfig {
            max_body_mb: 1,
            ..Default::default()
        },
        ..Config::default()
    };
    let edge = serve(config).await;

    let before = status_of(&edge, None).await;
    assert_eq!(before["turns"], json!(0), "nothing has been asked yet");

    // No key, but a readable body: the refusal is about the key.
    let response = client()
        .post(format!("{edge}/v1/chat/completions"))
        .json(&json!({ "model": "m", "messages": [] }))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), 401);

    // Not JSON, and no key either: the refusal is about the body.
    let response = client()
        .post(format!("{edge}/v1/chat/completions"))
        .header("content-type", "application/json")
        .body("{ not json")
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), 400);

    // Over the ceiling: refused before it was decoded.
    let huge = "x".repeat(2 * 1024 * 1024);
    let response = client()
        .post(format!("{edge}/v1/chat/completions"))
        .header("authorization", "Bearer user_abc123")
        .json(&json!({ "model": "m", "messages": [{ "role": "user", "content": huge }] }))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), 413);

    // And one that works.
    let response = client()
        .post(format!("{edge}/v1/chat/completions"))
        .header("authorization", "Bearer user_abc123")
        .json(&json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }] }))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), 200);

    let after = status_of(&edge, None).await;
    assert_eq!(after["unauthenticated"], json!(1), "one request arrived without a key");
    assert_eq!(after["malformed"], json!(1), "one body was not JSON");
    assert_eq!(after["too_large"], json!(1), "one body was over the ceiling");
    assert_eq!(after["turns"], json!(1), "one request became a turn");
    assert_eq!(after["upstream_failed"], json!(0), "the upstream answered every time");
    assert_eq!(after["refused"], json!(0), "the ceiling was never reached");
    assert_eq!(after["timeouts"], json!(0));
    assert_eq!(after["client_stalls"], json!(0));
    assert_eq!(after["inflight"], json!(0), "every request finished");
    assert!(
        after["uptime_ms"].as_u64().expect("uptime") >= 1,
        "the clock is running"
    );
    assert_eq!(
        after["max_inflight"], before["max_inflight"],
        "the ceiling is the deployment's"
    );
}

/// `/status` is aggregate, which is what lets it be served without a key — and a
/// body served to anyone must carry nothing that belongs to a caller.
///
/// A count is about this process; a key, a fingerprint and a model name are about
/// this process's traffic. The difference is the whole reason the route needs no
/// credential, so the test drives a turn through with distinctive values and then
/// reads the raw body looking for them.
#[tokio::test]
async fn the_counts_are_readable_without_a_key_and_name_nothing_private() {
    let (upstream, _) = upstream(|| (200, DELTAS.to_owned(), "application/x-ndjson")).await;
    let edge = edge(&upstream).await;

    let response = client()
        .post(format!("{edge}/v1/chat/completions"))
        .header("authorization", "Bearer user_canary_key")
        .json(&json!({
            "model": "model_canary",
            "messages": [{ "role": "user", "content": "canary_prompt" }]
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), 200);

    let response = client().get(format!("{edge}/status")).send().await.expect("send");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");

    assert!(
        !body.contains("user_canary_key"),
        "the counts must not quote a key: {body}"
    );
    assert!(
        !body.contains(&key_fingerprint("user_canary_key")),
        "nor a fingerprint of one: {body}"
    );
    assert!(
        !body.contains("model_canary"),
        "the counts must not quote a model: {body}"
    );
    assert!(!body.contains("canary_prompt"), "and never a body: {body}");
    assert_eq!(
        parsed_tokens(&body),
        json!([]),
        "and a deployment that issues no tokens names none"
    );

    let parsed: Value = serde_json::from_str(&body).expect("json");
    let mut fields: Vec<&str> = parsed.as_object().expect("object").keys().map(String::as_str).collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        [
            "client_stalls",
            "inflight",
            "malformed",
            "max_inflight",
            "refused",
            "timeouts",
            "tokens",
            "too_large",
            "turns",
            "unauthenticated",
            "upstream_failed",
            "uptime_ms",
        ],
        "the field list is the contract an operator's dashboard reads"
    );
    assert_eq!(parsed["turns"], json!(1), "the turn above worked");
}

/// `/v1/messages` end to end, which nothing exercised before: the only test that
/// touched the route checked its 401, so a break in the success path would have
/// left every test green.
#[tokio::test]
async fn a_messages_turn_crosses_the_anthropic_path() {
    let (upstream, received) = upstream(|| {
        (
            200,
            "{\"type\":\"text-delta\",\"text\":\"Hello\"}\n{\"type\":\"finish\",\"finishReason\":\"stop\",\"totalUsage\":{\"inputTokens\":10,\"outputTokens\":3}}\n".to_owned(),
            "application/x-ndjson",
        )
    })
    .await;
    let edge = edge(&upstream).await;

    let response = client()
        .post(format!("{edge}/v1/messages"))
        .header("x-api-key", "user_abc123")
        .json(&json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 64,
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["type"], json!("message"), "the Anthropic envelope");
    assert_eq!(body["role"], json!("assistant"));
    assert_eq!(body["model"], json!("claude-sonnet-4-6"));
    assert_eq!(body["content"][0], json!({ "type": "text", "text": "Hello" }));
    assert_eq!(body["stop_reason"], json!("end_turn"));
    assert!(
        body["id"].as_str().expect("id").starts_with("msg_"),
        "an Anthropic response is delivered under an Anthropic id, not chatcmpl-"
    );
    assert!(body["usage"]["output_tokens"].is_number());

    // The same envelope reaches the upstream whichever endpoint the client used.
    let received = received.lock().expect("record");
    let generate = received.last(GENERATE);
    assert_eq!(generate.header("authorization").as_deref(), Some("Bearer user_abc123"));
    assert_eq!(
        generate.body["params"]["messages"][0],
        json!({ "role": "user", "content": [{ "type": "text", "text": "hi" }] }),
        "a bare string on the Anthropic side still becomes content blocks on the wire"
    );
}

/// `/v1/messages` with `stream: true`: the frames an Anthropic SDK parses.
#[tokio::test]
async fn a_messages_stream_carries_anthropic_events() {
    let (upstream, _) = upstream(|| (200, DELTAS.to_owned(), "application/x-ndjson")).await;
    let edge = edge(&upstream).await;

    let response = client()
        .post(format!("{edge}/v1/messages"))
        .header("x-api-key", "user_abc123")
        .json(&json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 64,
            "messages": [{ "role": "user", "content": "hi" }],
            "stream": true
        }))
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .expect("content type")
            .to_str()
            .expect("ascii"),
        "text/event-stream"
    );

    let body = response.text().await.expect("body");
    assert!(
        body.starts_with("event: message_start\n"),
        "an Anthropic stream opens with message_start, not an OpenAI chunk: {body:.120}"
    );
    assert!(body.contains("event: content_block_delta\n"), "{body:.200}");
    assert!(body.contains("event: message_stop\n"), "{body:.200}");
    assert!(
        !body.contains("\"object\":\"chat.completion.chunk\""),
        "an Anthropic stream must not carry OpenAI chunk objects"
    );
}

/// `/v1/responses` end to end. The route was registered and never called by a
/// test, so only this exercises its wiring.
#[tokio::test]
async fn a_responses_turn_crosses_the_responses_path() {
    let (upstream, received) = upstream(|| {
        (
            200,
            "{\"type\":\"text-delta\",\"text\":\"Hello\"}\n{\"type\":\"finish\",\"finishReason\":\"stop\",\"totalUsage\":{\"inputTokens\":10,\"outputTokens\":3}}\n".to_owned(),
            "application/x-ndjson",
        )
    })
    .await;
    let edge = edge(&upstream).await;

    let response = client()
        .post(format!("{edge}/v1/responses"))
        .header("authorization", "Bearer user_abc123")
        .json(&json!({ "model": "gpt-5.5", "input": "hi" }))
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["object"], json!("response"));
    assert_eq!(body["status"], json!("completed"));
    assert_eq!(body["output_text"], json!("Hello"), "the convenience field");
    assert_eq!(body["output"][0]["type"], json!("message"));
    assert_eq!(
        body["output"][0]["content"][0],
        json!({ "type": "output_text", "text": "Hello", "annotations": [] })
    );
    assert_eq!(body["model"], json!("gpt-5.5"));
    assert!(
        body["id"].as_str().expect("id").starts_with("resp_"),
        "a Responses id is resp_-prefixed"
    );

    // A string input becomes the user turn the wire carries.
    let received = received.lock().expect("record");
    let generate = received.last(GENERATE);
    assert_eq!(
        generate.body["params"]["messages"][0],
        json!({ "role": "user", "content": [{ "type": "text", "text": "hi" }] })
    );
}

/// `/v1/responses` with `stream: true`: the named events its SDK parses.
#[tokio::test]
async fn a_responses_stream_carries_response_events() {
    let (upstream, _) = upstream(|| (200, DELTAS.to_owned(), "application/x-ndjson")).await;
    let edge = edge(&upstream).await;

    let response = client()
        .post(format!("{edge}/v1/responses"))
        .header("authorization", "Bearer user_abc123")
        .json(&json!({ "model": "gpt-5.5", "input": "hi", "stream": true }))
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("body");
    assert!(body.starts_with("event: response.created\n"), "{body:.120}");
    assert!(body.contains("event: response.output_text.delta\n"), "{body:.240}");
    assert!(
        body.contains("event: response.completed\n"),
        "a finished stream ends with response.completed: {body:.240}"
    );
    assert!(
        !body.contains("data: [DONE]"),
        "the Responses protocol names its terminal event rather than sending [DONE]"
    );
}

/// Admission control, which the configuration advertises and nothing tested.
/// With a ceiling of one, a second turn arriving while the first is in flight is
/// refused rather than queued.
#[tokio::test]
async fn a_turn_over_the_ceiling_is_refused_as_retryable() {
    // The upstream never answers, so the first turn is still in flight — holding
    // the one permit — when the second arrives. Nothing here blocks a thread: the
    // first turn is waiting on a socket, which is what makes it a fair model of a
    // deployment at its ceiling.
    let upstream = stalling_upstream().await;
    let edge = serve(Config {
        limits: LimitsConfig {
            max_inflight: 1,
            nonstream_idle_ms: 400,
            ..LimitsConfig::default()
        },
        ..Config {
            api_base: upstream.clone(),
            ..Config::default()
        }
    })
    .await;

    let turn = || {
        client()
            .post(format!("{edge}/v1/chat/completions"))
            .header("authorization", "Bearer user_abc123")
            .json(&json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }] }))
            .send()
    };

    let (first, second) = tokio::join!(turn(), async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        turn().await
    });

    let second = second.expect("send");
    assert_eq!(
        second.status(),
        503,
        "a turn over the ceiling is a retryable 503, not a queue"
    );
    assert_eq!(second.headers().get("retry-after").expect("hint"), "5");
    let body: Value = second.json().await.expect("json");
    assert_eq!(body["error"]["type"], json!("temporarily_unavailable"));

    // The first turn was admitted and then timed out on its own, which is what
    // says the second was refused for the ceiling rather than for something else.
    assert_eq!(first.expect("send").status(), 429);

    // And the counts say the same thing over HTTP, which is how an operator reads
    // it: one turn over the ceiling, none answered, and the timeout that released
    // the permit. Without the last one a busy deployment and a broken one look
    // alike from here.
    let status = status_of(&edge, None).await;
    assert_eq!(status["refused"], json!(1), "the second turn was over the ceiling");
    assert_eq!(status["turns"], json!(0), "neither turn produced an answer");
    assert_eq!(status["timeouts"], json!(1), "the admitted turn timed out on its own");
}

#[tokio::test]
async fn a_complete_answer_crosses_both_conversions() {
    let (upstream, received) = upstream(|| {
        (
            200,
            "{\"type\":\"text-delta\",\"text\":\"Hello\"}\n{\"type\":\"finish\",\"finishReason\":\"stop\",\"totalUsage\":{\"inputTokens\":10,\"outputTokens\":3}}\n".to_owned(),
            "application/x-ndjson",
        )
    })
    .await;
    let edge = edge(&upstream).await;

    let response = client()
        .post(format!("{edge}/v1/chat/completions"))
        .header("authorization", "Bearer user_abc123")
        .json(&json!({ "model": "deepseek/deepseek-v4-flash", "messages": [{ "role": "user", "content": "hi" }] }))
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["object"], json!("chat.completion"));
    assert_eq!(body["choices"][0]["message"]["content"], json!("Hello"));
    assert_eq!(body["usage"]["completion_tokens"], json!(3));

    // The upstream saw a real envelope: the key it was called with, and the
    // session and project headers the dialect requires.
    let received = received.lock().expect("record");
    let generate = received.last(GENERATE);
    let header = |name: &str| generate.header(name);
    assert_eq!(header("authorization").as_deref(), Some("Bearer user_abc123"));
    assert_eq!(header("x-command-code-version").as_deref(), Some("1.53.1"));
    assert!(header("traceparent").is_some(), "a trace id is always sent");
    assert_eq!(
        generate.body["params"]["messages"][0],
        json!({ "role": "user", "content": [{ "type": "text", "text": "hi" }] }),
        "the wire carries content blocks, not the flat string the client sent"
    );
}

/// The edge, with one deployment's model rules in force.
///
/// The rules are the shape a person writes for a client that asks by version: a
/// family prefix for the models that have to go somewhere, and the one exact name
/// in that family that has somewhere else to go.
fn aliasing(api_base: &str) -> Config {
    Config {
        api_base: api_base.to_owned(),
        models: ModelsConfig {
            aliases: [
                ("claude-".to_owned(), "deepseek/deepseek-v4-flash".to_owned()),
                ("claude-sonnet-5".to_owned(), "deepseek/deepseek-v4-pro".to_owned()),
            ]
            .into_iter()
            .collect(),
            ..ModelsConfig::default()
        },
        ..Config::default()
    }
}

/// A client that names a model this account is not served is pointed at one it is,
/// by a rule the deployment wrote rather than by a guess.
///
/// The rule rewrites the request itself, which is the only way the three names a
/// turn involves can agree: the model the upstream is asked for, the model the
/// response reports, and the model the access line records as used.
#[tokio::test]
async fn a_model_rule_rewrites_the_model_the_upstream_is_asked_for() {
    let (upstream, received) = upstream(|| (200, DELTAS.to_owned(), "application/x-ndjson")).await;
    let edge = serve(aliasing(&upstream)).await;

    let response = client()
        .post(format!("{edge}/v1/messages"))
        .header("x-api-key", "user_abc123")
        .json(&json!({
            "model": "claude-haiku-4-5",
            "max_tokens": 64,
            "messages": [{ "role": "user", "content": "hi" }],
        }))
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("json");
    assert_eq!(
        body["model"],
        json!("deepseek/deepseek-v4-flash"),
        "the answer is attributed to the model that produced it, not to the one that was asked for"
    );

    let received = received.lock().expect("record");
    assert_eq!(
        received.last(GENERATE).body["params"]["model"],
        json!("deepseek/deepseek-v4-flash")
    );
}

/// The longest rule that covers a name answers for it, so a family prefix does not
/// swallow the member of that family which has a rule of its own.
#[tokio::test]
async fn the_most_specific_model_rule_is_the_one_that_answers() {
    let (upstream, received) = upstream(|| (200, DELTAS.to_owned(), "application/x-ndjson")).await;
    let edge = serve(aliasing(&upstream)).await;

    let response = client()
        .post(format!("{edge}/v1/chat/completions"))
        .header("authorization", "Bearer user_abc123")
        .json(&json!({ "model": "claude-sonnet-5", "messages": [{ "role": "user", "content": "hi" }] }))
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["model"], json!("deepseek/deepseek-v4-pro"));
    let received = received.lock().expect("record");
    assert_eq!(
        received.last(GENERATE).body["params"]["model"],
        json!("deepseek/deepseek-v4-pro")
    );
}

/// A name no rule covers goes out as the client spelled it, and the upstream's
/// refusal is the client's answer: a rule table is not a default model, and a
/// substitution made here would be an answer to a question nobody asked.
#[tokio::test]
async fn an_unmapped_model_is_forwarded_and_refused_by_the_upstream() {
    let (upstream, received) = upstream(|| {
        (
            400,
            json!({
                "success": false,
                "error": { "code": "MODEL_NOT_IN_PLAN", "message": "gpt-5.4-mini is not in this plan" },
            })
            .to_string(),
            "application/json",
        )
    })
    .await;
    let edge = serve(aliasing(&upstream)).await;

    let response = client()
        .post(format!("{edge}/v1/chat/completions"))
        .header("authorization", "Bearer user_abc123")
        .json(&json!({ "model": "gpt-5.4-mini", "messages": [{ "role": "user", "content": "hi" }] }))
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], json!("MODEL_NOT_IN_PLAN"));
    let received = received.lock().expect("record");
    assert_eq!(
        received.last(GENERATE).body["params"]["model"],
        json!("gpt-5.4-mini"),
        "the upstream was asked about the model the client named"
    );
}

#[tokio::test]
async fn a_stream_is_committed_by_its_first_visible_frame() {
    let (upstream, _) = upstream(|| (200, DELTAS.to_owned(), "application/x-ndjson")).await;
    let edge = edge(&upstream).await;

    let response = client()
        .post(format!("{edge}/v1/chat/completions"))
        .header("authorization", "Bearer user_abc123")
        .json(&json!({
            "model": "deepseek/deepseek-v4-flash",
            "stream": true,
            "messages": [{ "role": "user", "content": "hi" }],
        }))
        .send()
        .await
        .expect("send");

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers().get("content-type").expect("content type"),
        "text/event-stream"
    );
    let body = response.text().await.expect("body");
    assert!(body.contains("\"content\":\"Hel\""), "{body}");
    assert!(
        body.contains("data: [DONE]"),
        "the stream ends with the sentinel: {body}"
    );
}

#[tokio::test]
async fn a_turn_that_produced_nothing_is_retryable_rather_than_empty() {
    let (upstream, _) = upstream(|| {
        (
            200,
            "{\"type\":\"finish\",\"finishReason\":\"stop\",\"totalUsage\":{\"inputTokens\":9,\"outputTokens\":0}}\n"
                .to_owned(),
            "application/x-ndjson",
        )
    })
    .await;
    let edge = edge(&upstream).await;

    let response = client()
        .post(format!("{edge}/v1/chat/completions"))
        .header("authorization", "Bearer user_abc123")
        .json(&json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }] }))
        .send()
        .await
        .expect("send");

    // The status is still available because nothing visible was ever produced,
    // which is the whole point of delaying the commit.
    assert_eq!(response.status(), 429);
    assert_eq!(response.headers().get("retry-after").expect("retry-after"), "10");
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["type"], json!("rate_limit_error"));
    assert_eq!(body["retry_after"], json!(10));
}

#[tokio::test]
async fn an_upstream_rejection_keeps_its_classification() {
    let (upstream, _) = upstream(|| {
        (
            402,
            json!({ "success": false, "error": { "code": "USAGE_EXCEEDED", "message": "out of quota" } }).to_string(),
            "application/json",
        )
    })
    .await;
    let edge = edge(&upstream).await;

    let response = client()
        .post(format!("{edge}/v1/chat/completions"))
        .header("authorization", "Bearer user_abc123")
        .json(&json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }] }))
        .send()
        .await
        .expect("send");

    // An exhausted quota is a retryable throttle, not a payment problem the
    // client cannot act on.
    assert_eq!(response.status(), 429);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["error"]["code"], json!("USAGE_EXCEEDED"));
    assert_eq!(body["retry_after"], json!(30));
}

/// Where a test's audit artifacts go.
fn temp_dir(label: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("bifrost-{}-{label}-{unique}", std::process::id()))
}

#[tokio::test]
async fn a_named_cache_key_becomes_a_breakpoint_only_when_asked_for() {
    for (enabled, expected) in [(false, false), (true, true)] {
        let (upstream, received) = upstream(|| (200, DELTAS.to_owned(), "application/x-ndjson")).await;
        let config = Config {
            api_base: upstream.clone(),
            mechanisms: MechanismsConfig {
                prompt_cache: enabled,
                ..Default::default()
            },
            ..Config::default()
        };
        let edge = serve(config).await;

        let response = client()
            .post(format!("{edge}/v1/chat/completions"))
            .header("authorization", "Bearer user_abc123")
            .json(&json!({
                "model": "m",
                "prompt_cache_key": "cache-abcdefgh",
                "messages": [
                    { "role": "system", "content": "you are a careful assistant" },
                    { "role": "user", "content": "hi" },
                ],
            }))
            .send()
            .await
            .expect("send");
        assert_eq!(response.status(), 200);

        let received = received.lock().expect("record");
        let envelope = received.last(GENERATE).body.to_string();
        assert_eq!(
            envelope.contains("cache_control"),
            expected,
            "prompt_cache={enabled}: {envelope}"
        );
    }
}

/// Start a fake upstream that accepts the request, answers with headers, and then
/// never sends a byte.
///
/// A stall is the only way to test a watchdog, and it has to be a real one: a
/// timeout asserted against a transport that cannot stall would be asserting the
/// test's own timer.
async fn stalling_upstream() -> String {
    let quiet = || async {
        let body = futures_util::stream::pending::<Result<axum::body::Bytes, std::convert::Infallible>>();
        (
            [(axum::http::header::CONTENT_TYPE, "application/x-ndjson")],
            axum::body::Body::from_stream(body),
        )
    };
    let router = axum::Router::new().fallback(quiet);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{address}")
}

#[tokio::test]
async fn a_quiet_upstream_is_reported_as_a_retryable_timeout() {
    let upstream = stalling_upstream().await;
    let config = Config {
        api_base: upstream,
        limits: LimitsConfig {
            nonstream_idle_ms: 100,
            ..Default::default()
        },
        ..Config::default()
    };
    let edge = serve(config).await;

    let response = client()
        .post(format!("{edge}/v1/chat/completions"))
        .header("authorization", "Bearer user_abc123")
        .json(&json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }] }))
        .send()
        .await
        .expect("send");

    // Retryable, and with a hint: a client that retries without one retries at full
    // speed into the same stall.
    assert_eq!(response.status(), 429);
    assert_eq!(response.headers().get("retry-after").expect("hint"), "5");
    let body: Value = response.json().await.expect("json");
    assert!(
        body["error"]["message"]
            .as_str()
            .expect("message")
            .starts_with("Response timeout"),
        "{body}"
    );
}

#[tokio::test]
async fn a_stream_that_never_committed_is_answered_with_a_status() {
    let upstream = stalling_upstream().await;
    let config = Config {
        api_base: upstream,
        limits: LimitsConfig {
            stream_idle_ms: 100,
            ..Default::default()
        },
        ..Config::default()
    };
    let edge = serve(config).await;

    let response = client()
        .post(format!("{edge}/v1/chat/completions"))
        .header("authorization", "Bearer user_abc123")
        .json(&json!({
            "model": "m",
            "stream": true,
            "messages": [{ "role": "user", "content": "hi" }],
        }))
        .send()
        .await
        .expect("send");
    assert_eq!(
        response.status(),
        429,
        "no frame was ever written, so a status was still available"
    );
}

/// Reports that the connection it rode in on was dropped.
struct Abandoned(Arc<AtomicBool>);

impl Drop for Abandoned {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// How much of an answer the upstream produces per read, in bytes.
const UPSTREAM_CHUNK: usize = 32 * 1024;

/// Start an upstream that sends one visible frame and then never stops, and report
/// whether the connection carrying it was abandoned.
///
/// The flag is the observable difference the stall watchdog makes: with one, the
/// connection is closed while the client is still connected; without one, it is held
/// until the client itself goes away.
///
/// Every chunk is a whole frame, as a real upstream's are. That matters: a chunk
/// that holds no complete line produces no output, and a gateway that keeps reading
/// in that case is buffering an answer it cannot forward — which is a different
/// failure, with a different fix.
async fn endless_upstream() -> (String, Arc<AtomicBool>) {
    let abandoned = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&abandoned);
    let router = axum::Router::new().fallback(move |request: axum::extract::Request| {
        let flag = Arc::clone(&flag);
        async move {
            // The announcement endpoints answer and finish, as a real upstream's do.
            // They have to: their connections are dropped as soon as the edge has
            // read a status, and if they rode on the endless guard below, that drop
            // would be indistinguishable from an abandoned generate.
            if request.uri().path() != GENERATE {
                return (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    axum::body::Body::from("{}"),
                )
                    .into_response();
            }
            // The guard rides in the stream's own state, so it is dropped exactly
            // when the body is — which is what hyper does when the client is gone.
            let state = (Abandoned(flag), 0u64);
            let body = futures_util::stream::unfold(state, |(guard, count)| async move {
                let frame = if count == 0 {
                    Bytes::from_static(b"{\"type\":\"text-delta\",\"text\":\"Hel\"}\n")
                } else {
                    let mut line = Vec::with_capacity(UPSTREAM_CHUNK + 32);
                    line.extend_from_slice(b"{\"type\":\"text-delta\",\"text\":\"");
                    line.resize(line.len() + UPSTREAM_CHUNK, b'x');
                    line.extend_from_slice(b"\"}\n");
                    Bytes::from(line)
                };
                Some((Ok::<Bytes, std::convert::Infallible>(frame), (guard, count + 1)))
            });
            (
                [(axum::http::header::CONTENT_TYPE, "application/x-ndjson")],
                axum::body::Body::from_stream(body),
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{address}"), abandoned)
}

/// Send one request and then stop taking bytes, holding the connection open.
///
/// A raw socket rather than an HTTP client on purpose: a client library keeps
/// reading into its own buffers, and what the watchdog reacts to is a peer that has
/// stopped taking bytes at all. The read before the silence is only enough to see
/// the response was committed.
///
/// The receive buffer is pinned small for the same reason: a default one on loopback
/// grows into the megabytes, so a client that has stopped reading still absorbs
/// hundreds of chunks — the writer keeps making progress and never looks stalled.
/// A tiny window turns "not reading" into a genuinely blocked writer within a few
/// chunks, which is the condition the watchdog exists to notice. Setting it
/// explicitly also disables receive-window autotuning, so the size is the size.
async fn read_the_head_then_go_quiet(address: std::net::SocketAddr) -> tokio::net::TcpStream {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let body = json!({
        "model": "m",
        "stream": true,
        "messages": [{ "role": "user", "content": "hi" }],
    })
    .to_string();
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nhost: {address}\r\n\
         authorization: Bearer user_abc123\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nconnection: keep-alive\r\n\r\n{body}",
        body.len()
    );

    let socket = tokio::net::TcpSocket::new_v4().expect("socket");
    socket.set_recv_buffer_size(4096).expect("receive buffer");
    let mut socket = socket.connect(address).await.expect("connect");
    socket.write_all(request.as_bytes()).await.expect("write");

    let mut head = [0u8; 4096];
    let read = socket.read(&mut head).await.expect("read");
    let head = String::from_utf8_lossy(&head[..read]);
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "the first frame commits the response: {head}"
    );

    // From here on the socket is held and never read again.
    socket
}

#[tokio::test]
async fn a_client_that_stops_reading_does_not_hold_the_upstream_open() {
    for (stall_ms, expected) in [(0u64, false), (200, true)] {
        let (upstream, abandoned) = endless_upstream().await;
        let config = Config {
            api_base: upstream.clone(),
            limits: LimitsConfig {
                client_stall_ms: stall_ms,
                ..Default::default()
            },
            ..Config::default()
        };
        let edge = serve(config).await;
        let address = edge.replace("http://", "");
        let address = address.parse().expect("edge address");
        let quiet = read_the_head_then_go_quiet(address).await;

        // Long enough for the socket and channel buffers to fill, and then for the
        // watchdog to decide the client has stopped.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(
            abandoned.load(Ordering::SeqCst),
            expected,
            "client_stall_ms={stall_ms}: nothing else closes the upstream while the client is still connected"
        );

        // The count has to follow the same switch: a deployment that did not ask
        // for a watchdog must not report breaches of one.
        let status = status_of(&edge, None).await;
        assert_eq!(
            status["client_stalls"],
            json!(u64::from(expected)),
            "client_stall_ms={stall_ms}: the count says whether the watchdog fired"
        );
        drop(quiet);
    }
}

/// An upstream that never terminates a line is cut off rather than buffered.
///
/// The decoder can render nothing until it sees a newline, so a line that never ends
/// is an answer the gateway can only accumulate. There is a ceiling on that, and
/// passing it fails the turn: the alternative is memory that grows with whatever the
/// upstream decides to send, which is the same unbounded growth the bounded channel
/// prevents on the socket side, reached through the decoder instead.
#[tokio::test]
async fn an_upstream_line_that_never_ends_is_not_buffered_forever() {
    // Past the ceiling, and small enough to stay a fast test.
    let unterminated = format!("{{\"type\":\"text-delta\",\"text\":\"{}\"", "x".repeat(2 * 1024 * 1024));
    let (upstream, _) = upstream(move || (200, unterminated.clone(), "application/x-ndjson")).await;
    let edge = edge(&upstream).await;

    let response = client()
        .post(format!("{edge}/v1/chat/completions"))
        .header("authorization", "Bearer user_abc123")
        .json(&json!({
            "model": "m",
            "stream": true,
            "messages": [{ "role": "user", "content": "hi" }],
        }))
        .send()
        .await
        .expect("send");

    assert_eq!(
        response.status(),
        502,
        "nothing was ever renderable, so the failure is reported as an upstream one"
    );
    let body: Value = response.json().await.expect("json");
    assert!(
        body["error"]["message"]
            .as_str()
            .expect("message")
            .contains("terminator"),
        "the reason has to name what the upstream did wrong: {body}"
    );
}

/// Send one streamed turn through `edge` and return its status.
async fn stream_a_turn(edge: &str) -> u16 {
    let response = client()
        .post(format!("{edge}/v1/chat/completions"))
        .header("authorization", "Bearer user_abc123")
        .json(&json!({
            "model": "m",
            "stream": true,
            "messages": [{ "role": "user", "content": "hi" }],
        }))
        .send()
        .await
        .expect("send");
    let status = response.status().as_u16();
    // The body is taken so the turn is over before the test looks at the upstream.
    let _ = response.bytes().await;
    status
}

/// The first turn of a window introduces the key; the turns after it do not.
///
/// Two records, one per key per window: the device the key is on, and the session
/// it exists in. Both are the same facts for every turn in the window, so sending
/// them per turn would be repetition, and sending them never — which is what a
/// gateway that only knows how to generate does — leaves the upstream with no
/// device for the key at all.
#[tokio::test]
async fn a_first_turn_announces_the_key_and_later_ones_do_not() {
    let (upstream, received) = upstream(|| (200, DELTAS.to_owned(), "application/x-ndjson")).await;
    let edge = edge(&upstream).await;

    for _ in 0..3 {
        assert_eq!(stream_a_turn(&edge).await, 200);
    }

    let received = received.lock().expect("record");
    assert_eq!(
        received.count(GENERATE),
        3,
        "every turn still generates: {}",
        received.seen()
    );

    let records = received.to("/alpha/fingerprint/record");
    assert_eq!(records.len(), 1, "one announcement per window: {}", received.seen());
    let record = records[0];
    assert_eq!(record.header("authorization").as_deref(), Some("Bearer user_abc123"));
    assert_eq!(record.header("x-command-code-version").as_deref(), Some("1.53.1"));
    assert_eq!(record.header("x-cli-environment").as_deref(), Some("production"));
    // The record is the identity the fingerprint module derives for this key, in
    // the shape its own collector produces — not a summary of it and not a
    // second derivation from the same inputs.
    assert_eq!(
        record.body,
        json!(generate_fingerprint("user_abc123", "", &DeviceProfile::default())),
        "the upstream is asked to remember exactly the identity this build derives"
    );

    let events = received.to("/alpha/lifecycle-events");
    assert_eq!(events.len(), 1);
    let event = events[0];
    assert_eq!(event.body["eventType"], json!("cli_session_exists"));
    let metadata = &event.body["metadata"];
    assert_eq!(metadata["cliVersion"], json!("1.53.1"));
    assert_eq!(metadata["mode"], json!("interactive"));
    assert_eq!(
        metadata["os"],
        json!("win32-x64"),
        "the same platform and architecture the envelope reports"
    );
    let session = metadata["sessionId"].as_str().expect("sessionId");
    assert!(session.starts_with("sess_"), "{session}");
    assert_eq!(session.len(), "sess_".len() + 16, "eight bytes of hex: {session}");

    // An announcement is not a turn: it carries none of a generate's own headers.
    assert!(event.header("traceparent").is_none());
    assert!(event.header("x-project-slug").is_none());
    assert!(event.header("x-session-id").is_none());
}

/// A deployment that does not announce makes no other call at all.
#[tokio::test]
async fn a_deployment_that_does_not_announce_makes_one_call_per_turn() {
    let (upstream, received) = upstream(|| (200, DELTAS.to_owned(), "application/x-ndjson")).await;
    let config = Config {
        api_base: upstream.clone(),
        mechanisms: MechanismsConfig {
            lifecycle: false,
            ..Default::default()
        },
        ..Config::default()
    };
    let edge = serve(config).await;

    assert_eq!(stream_a_turn(&edge).await, 200);
    assert_eq!(stream_a_turn(&edge).await, 200);

    let received = received.lock().expect("record");
    assert_eq!(
        received.seen(),
        format!("{GENERATE}, {GENERATE}"),
        "the switch has to mean nothing else is sent, not that it is sent less often"
    );
}

/// The session event does not depend on the device being reported.
#[tokio::test]
async fn the_device_is_left_unrecorded_when_the_fingerprint_is_off() {
    let (upstream, received) = upstream(|| (200, DELTAS.to_owned(), "application/x-ndjson")).await;
    let config = Config {
        api_base: upstream.clone(),
        mechanisms: MechanismsConfig {
            fingerprint: false,
            ..Default::default()
        },
        ..Config::default()
    };
    let edge = serve(config).await;

    assert_eq!(stream_a_turn(&edge).await, 200);

    let received = received.lock().expect("record");
    assert_eq!(received.count("/alpha/fingerprint/record"), 0);
    assert_eq!(
        received.count("/alpha/lifecycle-events"),
        1,
        "a session exists whether or not a device is reported: {}",
        received.seen()
    );
}

/// An announcement the upstream refuses is not a reason to refuse the turn.
///
/// The two are different questions — the record is about a key, the turn is about
/// a request — and an upstream that dislikes the record still answers the
/// generate. Failing the turn because a record was rejected would take traffic
/// down over bookkeeping.
#[tokio::test]
async fn an_announcement_the_upstream_refuses_does_not_fail_the_turn() {
    let (upstream, received) = routed_upstream(|path| {
        if path == GENERATE {
            (200, DELTAS.to_owned(), "application/x-ndjson")
        } else {
            (500, "{}".to_owned(), "application/json")
        }
    })
    .await;
    let edge = edge(&upstream).await;

    assert_eq!(stream_a_turn(&edge).await, 200);

    let received = received.lock().expect("record");
    assert_eq!(received.count("/alpha/fingerprint/record"), 1, "it was attempted");
    assert_eq!(received.count(GENERATE), 1);
}

/// A pre-flight that never answers does not hold the turn.
///
/// The turn waits for the announcement, because the record is worth having before
/// the generate — but not forever. An upstream that accepts the announcement and
/// then says nothing is the case that has to end in a served request rather than a
/// request that never finishes.
#[tokio::test]
async fn a_pre_flight_that_never_answers_does_not_hold_the_turn() {
    let router = axum::Router::new().fallback(move |request: axum::extract::Request| async move {
        if request.uri().path() != GENERATE {
            // Never answered at all: not a body that stalls, but a request with no
            // reply, which is the shape of a pre-flight that hangs.
            return std::future::pending::<axum::response::Response>().await;
        }
        ([(axum::http::header::CONTENT_TYPE, "application/x-ndjson")], DELTAS).into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let config = Config {
        api_base: format!("http://{address}"),
        limits: LimitsConfig {
            announce_ms: 100,
            ..Default::default()
        },
        ..Config::default()
    };
    let edge = serve(config).await;

    assert_eq!(
        stream_a_turn(&edge).await,
        200,
        "the announcement was abandoned and the turn was served"
    );
}

/// Fetch `/v1/models` and return the ids it offered, checking the envelope shape.
async fn catalogue_of(edge: &str, key: Option<&str>) -> Vec<String> {
    let mut request = client().get(format!("{edge}/v1/models"));
    if let Some(key) = key {
        request = request.header("authorization", format!("Bearer {key}"));
    }
    let response = request.send().await.expect("send");
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["object"], json!("list"));
    body["data"]
        .as_array()
        .expect("data")
        .iter()
        .map(|model| {
            assert_eq!(model["object"], json!("model"));
            assert_eq!(model["owned_by"], json!("command-code"));
            model["id"].as_str().expect("id").to_owned()
        })
        .collect()
}

/// A catalogue the upstream serves is the one a client is offered.
#[tokio::test]
async fn the_catalogue_comes_from_the_upstream_when_it_has_one() {
    let (upstream, received) = routed_upstream(|path| {
        if path == CATALOGUE {
            (
                200,
                r#"{"object":"list","data":[{"id":"alpha"},{"id":"beta"}]}"#.to_owned(),
                "application/json",
            )
        } else {
            (200, DELTAS.to_owned(), "application/x-ndjson")
        }
    })
    .await;
    let edge = edge(&upstream).await;

    assert_eq!(catalogue_of(&edge, Some("user_abc123")).await, vec!["alpha", "beta"]);

    let received = received.lock().expect("record");
    let lookup = received.last(CATALOGUE);
    assert_eq!(lookup.header("authorization").as_deref(), Some("Bearer user_abc123"));
    assert_eq!(lookup.header("x-command-code-version").as_deref(), Some("1.53.1"));
    assert_eq!(lookup.header("x-cli-environment").as_deref(), Some("production"));
    assert!(
        lookup.header("content-length").is_none(),
        "a lookup is a GET with no body at all, not one carrying `null`"
    );
    assert!(
        lookup.header("traceparent").is_none(),
        "a catalogue is about an account, not a turn"
    );
}

/// The catalogue is fetched once and then answered from memory.
#[tokio::test]
async fn the_catalogue_is_fetched_once_and_reused() {
    let (upstream, received) = routed_upstream(|path| {
        if path == CATALOGUE {
            (200, r#"{"data":[{"id":"alpha"}]}"#.to_owned(), "application/json")
        } else {
            (200, DELTAS.to_owned(), "application/x-ndjson")
        }
    })
    .await;
    let edge = edge(&upstream).await;

    for _ in 0..3 {
        assert_eq!(catalogue_of(&edge, Some("user_abc123")).await, vec!["alpha"]);
    }
    assert_eq!(received.lock().expect("record").count(CATALOGUE), 1);
}

/// A refused catalogue falls back to the table this build was tested against.
#[tokio::test]
async fn a_refused_catalogue_falls_back_to_the_built_in_table() {
    let (upstream, received) = routed_upstream(|path| {
        if path == CATALOGUE {
            (500, "{}".to_owned(), "application/json")
        } else {
            (200, DELTAS.to_owned(), "application/x-ndjson")
        }
    })
    .await;
    let edge = edge(&upstream).await;

    assert_eq!(
        catalogue_of(&edge, Some("user_abc123")).await,
        BUILT_IN.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>()
    );
    // Nothing is cached from a refusal, so the next call asks again rather than
    // serving the fallback for the rest of the window.
    assert_eq!(catalogue_of(&edge, Some("user_abc123")).await.len(), BUILT_IN.len());
    assert_eq!(received.lock().expect("record").count(CATALOGUE), 2);
}

/// A body that is not a catalogue is not an answer either.
#[tokio::test]
async fn a_body_that_is_not_a_catalogue_falls_back_to_the_built_in_table() {
    let (upstream, _) = routed_upstream(|path| {
        if path == CATALOGUE {
            (200, "<html>gateway timeout</html>".to_owned(), "text/html")
        } else {
            (200, DELTAS.to_owned(), "application/x-ndjson")
        }
    })
    .await;
    let edge = edge(&upstream).await;

    assert_eq!(catalogue_of(&edge, Some("user_abc123")).await.len(), BUILT_IN.len());
}

/// A deployment that does not ask its upstream never calls it.
#[tokio::test]
async fn a_deployment_that_does_not_ask_serves_the_built_in_table() {
    let (upstream, received) = upstream(|| (200, DELTAS.to_owned(), "application/x-ndjson")).await;
    let config = Config {
        api_base: upstream.clone(),
        models: ModelsConfig {
            provider: false,
            ..Default::default()
        },
        ..Config::default()
    };
    let edge = serve(config).await;

    assert_eq!(catalogue_of(&edge, Some("user_abc123")).await.len(), BUILT_IN.len());
    let received = received.lock().expect("record");
    assert_eq!(
        received.count(CATALOGUE),
        0,
        "the switch means no call: {}",
        received.seen()
    );
}

/// A client that has not identified itself is served the table, not the account's.
#[tokio::test]
async fn an_unauthenticated_catalogue_request_serves_the_built_in_table() {
    let (upstream, received) = routed_upstream(|path| {
        if path == CATALOGUE {
            (200, r#"{"data":[{"id":"alpha"}]}"#.to_owned(), "application/json")
        } else {
            (200, DELTAS.to_owned(), "application/x-ndjson")
        }
    })
    .await;
    let edge = edge(&upstream).await;

    assert_eq!(catalogue_of(&edge, None).await.len(), BUILT_IN.len());
    assert_eq!(
        received.lock().expect("record").count(CATALOGUE),
        0,
        "there is no key to ask with"
    );
}

/// A refresh that fails serves the last catalogue the upstream gave.
///
/// The alternative is the built-in table, which is a list from this build's own
/// past rather than one from the service. `refresh_ms = 0` makes every request a
/// refresh, which is what puts the two answers side by side.
#[tokio::test]
async fn a_failed_refresh_serves_the_last_catalogue() {
    let answering = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&answering);
    let (upstream, received) = routed_upstream(move |path| {
        if path != CATALOGUE {
            return (200, DELTAS.to_owned(), "application/x-ndjson");
        }
        if flag.load(Ordering::SeqCst) {
            (
                200,
                r#"{"data":[{"id":"from-provider"}]}"#.to_owned(),
                "application/json",
            )
        } else {
            (500, "{}".to_owned(), "application/json")
        }
    })
    .await;
    let config = Config {
        api_base: upstream.clone(),
        models: ModelsConfig {
            refresh_ms: 0,
            ..Default::default()
        },
        ..Config::default()
    };
    let edge = serve(config).await;

    assert_eq!(catalogue_of(&edge, Some("user_abc123")).await, vec!["from-provider"]);

    answering.store(false, Ordering::SeqCst);
    assert_eq!(
        catalogue_of(&edge, Some("user_abc123")).await,
        vec!["from-provider"],
        "a list the upstream produced is better evidence than one compiled in"
    );
    assert_eq!(received.lock().expect("record").count(CATALOGUE), 2, "it did try again");
}

/// The working directory is one setting with several consequences: the envelope's
/// working directory, the project slug derived from it, and the device the
/// fingerprint describes. A real client cannot produce a combination of those that
/// disagree, so neither may this.
#[tokio::test]
async fn the_configured_working_directory_is_the_one_the_upstream_sees() {
    const FINGERPRINT: &str = "/alpha/fingerprint/record";
    let (upstream, received) = upstream(|| (200, DELTAS.to_owned(), "application/x-ndjson")).await;
    let directory = "D:\\builds\\checkout";
    let config = Config {
        api_base: upstream.clone(),
        device: DeviceConfig {
            project_dir: Some(directory.to_owned()),
        },
        ..Config::default()
    };
    let edge = serve(config).await;

    let response = client()
        .post(format!("{edge}/v1/chat/completions"))
        .header("authorization", "Bearer user_abc123")
        .json(&json!({ "model": "deepseek/deepseek-v4-flash", "messages": [{ "role": "user", "content": "hi" }] }))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), 200);
    let _ = response.text().await;

    let received = received.lock().expect("record");
    let generate = received.last(GENERATE);
    assert_eq!(generate.body.pointer("/config/workingDir"), Some(&json!(directory)));
    assert_eq!(
        generate.header("x-project-slug").as_deref(),
        Some("d-builds-checkout"),
        "the slug is the directory, not something set beside it"
    );

    let announced = received.last(FINGERPRINT);
    let expected = generate_fingerprint(
        "user_abc123",
        "",
        &DeviceProfile {
            project_dir: directory.to_owned(),
            ..DeviceProfile::default()
        },
    );
    assert_eq!(
        announced.body,
        serde_json::to_value(expected).expect("json"),
        "the device that was announced is the one the profile describes"
    );
}

/// A deployment that does not report sessions sends none, and a client naming
/// one does not override that: the switch is about what leaves this process.
#[tokio::test]
async fn a_deployment_without_sessions_reports_none() {
    let (upstream, received) = upstream(|| (200, DELTAS.to_owned(), "application/x-ndjson")).await;
    let config = Config {
        api_base: upstream.clone(),
        mechanisms: MechanismsConfig {
            session: false,
            ..Default::default()
        },
        ..Config::default()
    };
    let edge = serve(config).await;

    let response = client()
        .post(format!("{edge}/v1/chat/completions"))
        .header("authorization", "Bearer user_abc123")
        .header("x-session-id", "a-session-the-client-named")
        .json(&json!({ "model": "deepseek/deepseek-v4-flash", "messages": [{ "role": "user", "content": "hi" }] }))
        .send()
        .await
        .expect("send");
    assert_eq!(response.status(), 200);
    let _ = response.text().await;

    let received = received.lock().expect("record");
    let generate = received.last(GENERATE);
    assert_eq!(
        generate.header("x-session-id"),
        None,
        "no session leaves the process, whoever named it"
    );
    assert_eq!(
        generate.body.get("threadId"),
        None,
        "and the envelope does not carry one either: {}",
        generate.body
    );
}

/// An edge that issues tokens, and the token it issued for `name`.
///
/// The key file is written rather than mocked because it is a file the process
/// reads: what is being tested is that the deployment serves with the key it was
/// given, and a test that handed the key to the edge another way would not be
/// testing that.
async fn issuing_edge(upstream: &str, root: &Path, name: &str, rpm: u32, concurrency: u32) -> (String, String) {
    issuing_edge_bounded(upstream, root, name, rpm, concurrency, LimitsConfig::default()).await
}

/// The same, for a test that needs the turn to end on its own quickly.
async fn issuing_edge_bounded(
    upstream: &str,
    root: &Path,
    name: &str,
    rpm: u32,
    concurrency: u32,
    limits: LimitsConfig,
) -> (String, String) {
    std::fs::create_dir_all(root).expect("scratch");
    let key_file = root.join("auth.json");
    std::fs::write(&key_file, r#"{"apiKey":"user_deployment_key","userName":"ops"}"#).expect("write the key");
    let tokens_file = root.join("tokens.json");
    let path = tokens_file.clone();
    let (edge, token) = issuing_edge_with(upstream, &key_file, path, name, rpm, concurrency, limits).await;
    (edge, token)
}

/// The same, for a test that keeps the token file itself.
async fn issuing_edge_with(
    upstream: &str,
    key_file: &Path,
    tokens_file: PathBuf,
    name: &str,
    rpm: u32,
    concurrency: u32,
    limits: LimitsConfig,
) -> (String, String) {
    let mut document = Document::default();
    let token = document.issue(name, rpm, concurrency).expect("issue");
    document.write(&tokens_file).expect("write the tokens");
    let edge = serve(Config {
        api_base: upstream.to_owned(),
        access: AccessConfig {
            enabled: true,
            key_file: Some(key_file.to_path_buf()),
            tokens_file,
        },
        limits,
        ..Config::default()
    })
    .await;
    (edge, token)
}

fn a_turn(edge: &str, credential: &str) -> reqwest::RequestBuilder {
    client()
        .post(format!("{edge}/v1/chat/completions"))
        .header("authorization", format!("Bearer {credential}"))
        .json(&json!({ "model": "deepseek/deepseek-v4-flash", "messages": [{ "role": "user", "content": "hi" }] }))
}

/// The point of issuing tokens: the caller spends the account, and the account's
/// key is nowhere the caller can reach it.
#[tokio::test]
async fn an_issued_token_serves_a_turn_with_the_deployment_key() {
    let (upstream, received) = upstream(|| (200, DELTAS.to_owned(), "application/x-ndjson")).await;
    let root = temp_dir("access-serves");
    let (edge, token) = issuing_edge(&upstream, &root, "laptop", 0, 0).await;

    let response = a_turn(&edge, &token).send().await.expect("send");
    assert_eq!(response.status(), 200);
    let _ = response.text().await;

    {
        let received = received.lock().expect("record");
        assert_eq!(
            received.last(GENERATE).header("authorization").as_deref(),
            Some("Bearer user_deployment_key"),
            "the upstream is spoken to with the key this deployment holds"
        );
    }

    let status = status_of(&edge, Some(&token)).await;
    assert_eq!(status["tokens"][0]["name"], json!("laptop"));
    assert_eq!(status["tokens"][0]["requests"], json!(1));
    assert_eq!(status["tokens"][0]["inflight"], json!(0));
    assert_eq!(status["tokens"][0]["revoked"], json!(false));
    assert_eq!(status["unauthenticated"], json!(0));
    let _ = std::fs::remove_dir_all(&root);
}

/// A key is not a token. The half of access control that makes the rest of it
/// mean something: a credential that still worked would be one no revocation
/// reaches, and the caller would never learn which deployment it was talking to.
#[tokio::test]
async fn a_key_is_not_a_token_where_tokens_are_issued() {
    let (upstream, received) = upstream(|| (200, DELTAS.to_owned(), "application/x-ndjson")).await;
    let root = temp_dir("access-key-pointing");
    let (edge, token) = issuing_edge(&upstream, &root, "laptop", 0, 0).await;

    let response = a_turn(&edge, "user_a_key_of_its_own").send().await.expect("send");
    assert_eq!(response.status(), 401);
    let body: Value = response.json().await.expect("json");
    assert!(
        body["error"]["message"]
            .as_str()
            .expect("message")
            .contains("issues its own tokens"),
        "the refusal says which deployment this is: {body}"
    );

    let response = a_turn(&edge, "bfr_not_a_token_that_was_issued")
        .send()
        .await
        .expect("send");
    assert_eq!(
        response.status(),
        401,
        "and so is one shaped like a token but not issued"
    );

    assert_eq!(
        received.lock().expect("record").count(GENERATE),
        0,
        "nothing reached the upstream"
    );
    assert_eq!(status_of(&edge, Some(&token)).await["unauthenticated"], json!(2));
    let _ = std::fs::remove_dir_all(&root);
}

/// The page names callers where tokens are issued, so it takes a credential.
///
/// A deployment that issues tokens is one that is more than a laptop — that is what
/// issuing is for — and the port it listens on is the thing that reaches further
/// than the machine. A list of who is using the account is not the thing to hand
/// out with it, so the rule is the one the turn endpoints already follow: a token,
/// or nothing, and a key is not a token.
#[tokio::test]
async fn the_page_that_names_callers_asks_for_a_credential() {
    let (upstream, received) = upstream(|| (200, DELTAS.to_owned(), "application/x-ndjson")).await;
    let root = temp_dir("access-status");
    let (edge, token) = issuing_edge(&upstream, &root, "laptop", 0, 0).await;

    let response = status_from(&edge, None).await;
    assert_eq!(response.status(), 401, "no credential, no list of callers");
    let body = response.text().await.expect("body");
    assert!(
        body.contains("token this deployment issued"),
        "the refusal says what to send instead: {body}"
    );
    assert!(!body.contains("laptop"), "and it names nobody: {body}");

    let response = status_from(&edge, Some("user_a_key_of_its_own")).await;
    assert_eq!(
        response.status(),
        401,
        "a key is not a token on the page either, which is what makes revocation mean something here"
    );

    let status = status_of(&edge, Some(&token)).await;
    assert_eq!(status["tokens"][0]["name"], json!("laptop"));
    assert_eq!(status["unauthenticated"], json!(2), "and both refusals are counted");
    assert_eq!(
        received.lock().expect("record").count(GENERATE),
        0,
        "neither read was a turn"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Revocation is the reason to issue tokens at all, so it is read from the file on
/// the request rather than at startup: a token that only stops working after a
/// restart is one that works until somebody remembers.
#[tokio::test]
async fn a_revoked_token_stops_working_at_once() {
    let (upstream, _) = upstream(|| (200, DELTAS.to_owned(), "application/x-ndjson")).await;
    let root = temp_dir("access-revoke");
    let key_file = root.join("auth.json");
    let tokens_file = root.join("tokens.json");
    std::fs::create_dir_all(&root).expect("scratch");
    std::fs::write(&key_file, r#"{"apiKey":"user_deployment_key"}"#).expect("write the key");
    let (edge, token) = issuing_edge_with(
        &upstream,
        &key_file,
        tokens_file.clone(),
        "phone",
        0,
        0,
        LimitsConfig::default(),
    )
    .await;

    assert_eq!(a_turn(&edge, &token).send().await.expect("send").status(), 200);

    // A second token, because reading the page takes one and the one above is
    // about to stop being a credential: a page that names callers is closed to a
    // caller who has been let go.
    let mut document = Document::read(&tokens_file).expect("read");
    let monitor = document.issue("monitor", 0, 0).expect("issue");
    document.revoke("phone").expect("revoke");
    document.write(&tokens_file).expect("write");

    assert_eq!(
        a_turn(&edge, &token).send().await.expect("send").status(),
        401,
        "the next request is refused"
    );
    assert_eq!(
        status_from(&edge, Some(&token)).await.status(),
        401,
        "and it reads no page either: the credential is gone, not just the turn"
    );
    let status = status_of(&edge, Some(&monitor)).await;
    let rows = status["tokens"].as_array().expect("rows");
    let phone = rows
        .iter()
        .find(|row| row["name"] == json!("phone"))
        .expect("the revoked token is still listed");
    assert_eq!(phone["revoked"], json!(true), "and the count says why");
    let _ = std::fs::remove_dir_all(&root);
}

/// A token issued while the deployment runs is usable without a restart, which is
/// the other half of the same mechanism.
#[tokio::test]
async fn a_token_issued_while_it_runs_is_picked_up() {
    let (upstream, _) = upstream(|| (200, DELTAS.to_owned(), "application/x-ndjson")).await;
    let root = temp_dir("access-issue-live");
    let key_file = root.join("auth.json");
    let tokens_file = root.join("tokens.json");
    std::fs::create_dir_all(&root).expect("scratch");
    std::fs::write(&key_file, r#"{"apiKey":"user_deployment_key"}"#).expect("write the key");
    let (edge, first) = issuing_edge_with(
        &upstream,
        &key_file,
        tokens_file.clone(),
        "laptop",
        0,
        0,
        LimitsConfig::default(),
    )
    .await;

    let mut document = Document::read(&tokens_file).expect("read");
    let second = document.issue("phone", 0, 0).expect("issue");
    document.write(&tokens_file).expect("write");

    assert_eq!(a_turn(&edge, &first).send().await.expect("send").status(), 200);
    assert_eq!(
        a_turn(&edge, &second).send().await.expect("send").status(),
        200,
        "a token issued after the process started works while it runs"
    );
    let status = status_of(&edge, Some(&second)).await;
    assert_eq!(status["tokens"].as_array().expect("rows").len(), 2);
    let _ = std::fs::remove_dir_all(&root);
}

/// A minute's allowance is a limit on the caller rather than on the account, and
/// it comes with the number the client should wait.
#[tokio::test]
async fn a_token_over_its_minute_is_told_to_wait() {
    let (upstream, received) = upstream(|| (200, DELTAS.to_owned(), "application/x-ndjson")).await;
    let root = temp_dir("access-rpm");
    let (edge, token) = issuing_edge(&upstream, &root, "laptop", 1, 0).await;

    assert_eq!(a_turn(&edge, &token).send().await.expect("send").status(), 200);
    let refused = a_turn(&edge, &token).send().await.expect("send");
    assert_eq!(refused.status(), 429, "the second request in the minute is over it");
    assert_eq!(
        refused.headers().get("retry-after").expect("hint"),
        "60",
        "a minute's bucket that is empty is a minute to wait"
    );

    assert_eq!(
        received.lock().expect("record").count(GENERATE),
        1,
        "a request refused for the limit never reached the upstream"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// One caller holding every place is the failure a per-token ceiling exists to
/// prevent, so the refusal is the same retryable answer the deployment's own
/// ceiling gives.
#[tokio::test]
async fn a_token_over_its_ceiling_is_refused_as_retryable() {
    let upstream = stalling_upstream().await;
    let root = temp_dir("access-concurrency");
    // The upstream never answers, so the first turn holds the token's only place
    // while the second arrives — and the idle window is short so the first turn
    // ends on its own rather than leaving the test waiting out the default.
    let (edge, token) = issuing_edge_bounded(
        &upstream,
        &root,
        "laptop",
        0,
        1,
        LimitsConfig {
            nonstream_idle_ms: 400,
            ..LimitsConfig::default()
        },
    )
    .await;

    let (first, second) = tokio::join!(a_turn(&edge, &token).send(), async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        a_turn(&edge, &token).send().await
    });

    let second = second.expect("send");
    assert_eq!(second.status(), 503);
    assert_eq!(second.headers().get("retry-after").expect("hint"), "5");
    let body: Value = second.json().await.expect("json");
    assert!(
        body["error"]["message"].as_str().expect("message").contains("laptop"),
        "the refusal names the token that is over its ceiling: {body}"
    );

    // The first was admitted and timed out on its own, which is what says the
    // second was refused for the token's ceiling rather than for something else.
    assert_eq!(first.expect("send").status(), 429);
    assert_eq!(status_of(&edge, Some(&token)).await["tokens"][0]["requests"], json!(1));
    let _ = std::fs::remove_dir_all(&root);
}

/// A deployment that issues tokens asks its own question of the catalogue, so an
/// anonymous `GET /v1/models` answers with this account's models rather than the
/// table compiled into the build.
#[tokio::test]
async fn the_catalogue_is_fetched_with_the_deployment_key() {
    let (upstream, received) = routed_upstream(|path| {
        if path == CATALOGUE {
            (
                200,
                r#"{"data":[{"id":"deepseek/deepseek-v4-flash"}]}"#.to_owned(),
                "application/json",
            )
        } else {
            (200, DELTAS.to_owned(), "application/x-ndjson")
        }
    })
    .await;
    let root = temp_dir("access-catalogue");
    let (edge, _) = issuing_edge(&upstream, &root, "laptop", 0, 0).await;

    let response = client().get(format!("{edge}/v1/models")).send().await.expect("send");
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["data"][0]["id"], json!("deepseek/deepseek-v4-flash"));

    let received = received.lock().expect("record");
    assert_eq!(
        received.last(CATALOGUE).header("authorization").as_deref(),
        Some("Bearer user_deployment_key"),
        "the fetch is the deployment's own call, made with the key it holds"
    );
    let _ = std::fs::remove_dir_all(&root);
}
