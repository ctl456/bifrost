//! The HTTP surface.
//!
//! Routing, admission and the three protocol endpoints. The endpoints differ
//! only in which adapter they name and how they spell an error, so they run
//! through one driver rather than three copies of it.

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bifrost_core::{Error, ErrorType};
use bifrost_protocol::adapter::{ProtocolAdapter, ResponseMeta};
use futures_util::StreamExt;
use serde_json::json;

use crate::error::{self, Shape};
use crate::evidence::{self, Captured, Evidence, TurnContext};
use crate::lifecycle;
use crate::log;
use crate::state::{Edge, InflightPermit, empty_output_error, idle_timeout_error, is_idle_timeout, now_unix};
use crate::stream::{self, Capture, Opening, Stall, StreamPlan};
use crate::upstream;

/// What this deployment records about a turn, and where.
///
/// Built once the request is known and handed down both answer paths, so that the
/// record is assembled from the same session and model the upstream was told
/// rather than reconstructed afterwards.
#[derive(Clone)]
struct Audit {
    evidence: Arc<Evidence>,
    turn: TurnContext,
    /// The archived client request, if it could be archived.
    request: Option<Captured>,
}

impl Audit {
    /// Keep a turn whose answer arrived whole.
    fn keep_response(self, status: u16, body: Bytes) {
        tokio::task::spawn_blocking(move || {
            let response = evidence::or_warn(self.evidence.archive_response(&body), "the upstream response");
            self.write(status, false, response);
        });
    }

    /// Keep a turn that never produced an answer.
    fn keep_failure(self, status: u16) {
        tokio::task::spawn_blocking(move || self.write(status, false, None));
    }

    fn write(&self, status: u16, stream: bool, response: Option<Captured>) {
        let turn = self.turn.turn(status, stream, self.request.clone(), response);
        let _ = evidence::or_warn(self.evidence.record(&turn), "the turn journal");
    }
}

/// One public endpoint.
struct Protocol {
    /// The adapter that speaks it.
    adapter: &'static str,
    /// How it spells errors.
    shape: Shape,
    /// Whether a quiet stream is kept alive with comment frames.
    heartbeat: bool,
}

const CHAT: Protocol = Protocol {
    adapter: bifrost_protocol::openai::chat::NAME,
    shape: Shape::OpenAi,
    heartbeat: false,
};

const MESSAGES: Protocol = Protocol {
    adapter: bifrost_protocol::anthropic::NAME,
    shape: Shape::Anthropic,
    // The Anthropic SDKs time out on a missing first byte, and a reasoning model
    // can think for minutes before it produces one.
    heartbeat: true,
};

const RESPONSES: Protocol = Protocol {
    adapter: bifrost_protocol::openai::responses::NAME,
    shape: Shape::Responses,
    heartbeat: false,
};

/// Build the application.
pub fn app(edge: Arc<Edge>) -> Router {
    Router::new()
        .route("/", get(liveness))
        .route("/health", get(liveness))
        .route("/status", get(status))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/messages", post(messages))
        .route("/v1/responses", post(responses))
        .fallback(not_found)
        .layer(axum::middleware::from_fn(cors))
        // Outermost, so that a request which never reaches a route — a 404, a
        // preflight — is still accounted for.
        .layer(axum::middleware::from_fn(access_log))
        .with_state(edge)
}

/// Write one line per request, whatever it answered.
///
/// The detail comes back in the response's extensions rather than being read here:
/// the request body is the turn's to consume, and a middleware that peeked at it
/// would either have to put it back or cost every request a copy.
async fn access_log(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    let started = std::time::Instant::now();
    let response = next.run(request).await;
    let detail = response.extensions().get::<log::Detail>().cloned().unwrap_or_default();
    log::access(&log::Access {
        method,
        path,
        status: response.status().as_u16(),
        ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        detail,
    });
    response
}

/// Cross-origin headers, on every response.
///
/// The proxy is meant to be reachable from a browser-based client, and an error
/// response without them is the one a developer most needs to read.
async fn cors(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    if request.method() == axum::http::Method::OPTIONS {
        let mut response = StatusCode::NO_CONTENT.into_response();
        add_cors(response.headers_mut());
        return response;
    }
    let mut response = next.run(request).await;
    add_cors(response.headers_mut());
    response
}

fn add_cors(headers: &mut HeaderMap) {
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("*"));
}

/// A liveness probe answers whether the process is up, nothing else.
///
/// Deliberately not the place the counts are reported: a probe is polled every few
/// seconds by something that wants one bit, and a body it has to parse is a body
/// every other health check in the deployment has to learn.
async fn liveness() -> Response {
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/plain")], "OK").into_response()
}

/// What this process has answered so far, as counts.
///
/// Read-only, aggregate and unauthenticated: every number is about this process
/// rather than about a client's traffic, and none of them names a key, a model or a
/// body. It answers the question an operator asks a deployment that is refusing
/// turns — is nothing arriving, or is nothing working — without anyone reading the
/// log. This is not the metrics switch that was removed: there is no switch, there
/// is nothing to configure, and no decision anywhere is taken from a number in it.
async fn status(State(edge): State<Arc<Edge>>) -> Response {
    let status = edge.status();
    json_response(
        StatusCode::OK,
        &json!({
            "uptime_ms": status.uptime_ms,
            "inflight": status.inflight,
            "max_inflight": status.max_inflight,
            "turns": status.turns,
            "refused": status.refused,
            "unauthenticated": status.unauthenticated,
            "too_large": status.too_large,
            "malformed": status.malformed,
            "upstream_failed": status.upstream_failed,
            "timeouts": status.timeouts,
            "client_stalls": status.client_stalls,
        }),
    )
}

async fn not_found() -> Response {
    error_response(Shape::OpenAi, &Error::not_found("Not found"))
}

async fn chat(State(edge): State<Arc<Edge>>, headers: HeaderMap, body: Body) -> Response {
    serve(edge, &CHAT, headers, body).await
}

async fn messages(State(edge): State<Arc<Edge>>, headers: HeaderMap, body: Body) -> Response {
    serve(edge, &MESSAGES, headers, body).await
}

async fn responses(State(edge): State<Arc<Edge>>, headers: HeaderMap, body: Body) -> Response {
    serve(edge, &RESPONSES, headers, body).await
}

/// The one path all three endpoints take.
///
/// The access line is written around this by [`access_log`], from whatever the
/// turn managed to learn before it answered.
async fn serve(edge: Arc<Edge>, protocol: &Protocol, headers: HeaderMap, body: Body) -> Response {
    let mut detail = log::Detail::default();
    let mut response = serve_turn(edge, protocol, headers, body, &mut detail).await;
    response.extensions_mut().insert(detail);
    response
}

async fn serve_turn(
    edge: Arc<Edge>,
    protocol: &Protocol,
    headers: HeaderMap,
    body: Body,
    detail: &mut log::Detail,
) -> Response {
    detail.protocol = Some(protocol.adapter.to_owned());
    let Some(adapter) = edge.adapter(protocol.adapter) else {
        return error_response(protocol.shape, &Error::internal("adapter is not registered"));
    };

    let payload = match read_body(body, edge.config().limits.max_body_bytes()).await {
        Ok(payload) => payload,
        Err(ReadError::TooLarge) => {
            let mb = edge.config().limits.max_body_mb;
            edge.note_too_large();
            return error_response(
                protocol.shape,
                &Error::payload_too_large(format!("Request body exceeds {mb}MB limit")),
            );
        }
        Err(ReadError::Unreadable) => {
            edge.note_malformed();
            return error_response(protocol.shape, &Error::invalid_request("Invalid JSON body"));
        }
    };

    // The key is checked after the body, so a malformed request is reported as
    // malformed rather than as unauthenticated — a client sent to fix its
    // credentials would never learn that its JSON was broken.
    let Some(api_key) = crate::auth::api_key(&headers) else {
        // Only the *choice* between the two errors needs the body parsed here.
        // When a key was sent, the adapter parses it anyway and reports the same
        // 400 for a body that is not JSON, so the normal path pays nothing.
        if serde_json::from_slice::<serde_json::Value>(&payload).is_err() {
            edge.note_malformed();
            return error_response(protocol.shape, &Error::invalid_request("Invalid JSON body"));
        }
        edge.note_unauthenticated();
        return error_response(
            protocol.shape,
            &Error::authentication("Missing API key. Send in Authorization: Bearer <key> or x-api-key header"),
        );
    };

    let decoded = match adapter.decode_request(&payload, &edge.convert_options()) {
        Ok(decoded) => decoded,
        Err(error) => {
            edge.note_malformed();
            return error_response(protocol.shape, &error.into_error());
        }
    };
    let request = decoded.value;
    // Recorded before admission, so a refused turn still says which model it was
    // refused for: that is the first thing an operator looks at when a deployment
    // is hitting its own ceiling.
    detail.model = Some(model_for(adapter, &request));
    detail.stream = Some(request.stream);

    let Some(admission) = admit(&edge) else {
        let ceiling = edge.config().limits.max_inflight;
        edge.note_refused();
        return error_response(
            protocol.shape,
            &Error {
                retry_after: Some(5),
                ..Error::unavailable(format!("Too many concurrent requests (limit {ceiling}), retry shortly"))
            },
        );
    };

    detail.key_fingerprint = Some(evidence::key_fingerprint(&api_key));
    let session = edge.session_for(&api_key, &headers, request.prompt_cache_key.as_deref());

    // Archiving starts only once the key has checked out: the archive is disk, and
    // a disk an unauthenticated caller can fill is a filling primitive rather than
    // evidence.
    let audit = edge.evidence().map(|evidence| Audit {
        evidence: Arc::clone(evidence),
        turn: TurnContext {
            protocol: protocol.adapter.to_owned(),
            model: model_for(adapter, &request),
            session: session.clone(),
            key_fingerprint: evidence::key_fingerprint(&api_key),
        },
        request: evidence::or_warn(evidence.archive_request(&payload), "the client request"),
    });

    let context = edge.wire_context(&api_key, &session);
    // The upstream is told which device this key is on, and that a session
    // exists, before it is asked to generate: the record is what makes the
    // identity observable, and the first turn of a window is the one that pays
    // for it. Nothing about it can fail the turn.
    lifecycle::announce(&edge, &api_key, &context).await;

    let encoded = match edge.encode(&request, &context) {
        Ok(encoded) => encoded,
        Err(error) => return error_response(protocol.shape, &error.into_error()),
    };

    let upstream = match upstream::send(&edge, &encoded).await {
        Ok(upstream) => upstream,
        Err(error) => {
            edge.note_upstream_failure();
            return error_response(protocol.shape, &error);
        }
    };

    if !upstream.is_success() {
        let status = upstream.status;
        let text = upstream.text().await;
        let error = upstream::classify(status, &text);
        edge.note_upstream_failure();
        if let Some(audit) = audit {
            audit.keep_failure(error.kind.status());
        }
        return error_response(protocol.shape, &error);
    }

    let response = match upstream.into_result().await {
        Ok(response) => response,
        Err(error) => {
            edge.note_upstream_failure();
            if let Some(audit) = audit {
                audit.keep_failure(error.kind.status());
            }
            return error_response(protocol.shape, &error);
        }
    };

    if !request.stream {
        return complete(adapter, protocol, &edge, &request, response, audit).await;
    }
    streaming(adapter, protocol, &edge, &request, response, admission, audit).await
}

/// A non-streaming answer.
async fn complete(
    adapter: &dyn ProtocolAdapter,
    protocol: &Protocol,
    edge: &Edge,
    request: &bifrost_core::CanonicalRequest,
    response: reqwest::Response,
    audit: Option<Audit>,
) -> Response {
    // The idle timeout covers the whole read rather than the gaps between reads.
    // There are no gaps to measure here: the upstream may answer in one piece, so
    // "no bytes for ninety seconds" and "no complete answer for ninety seconds"
    // are the same statement.
    let complete =
        match tokio::time::timeout(edge.config().limits.nonstream_idle(), upstream::read_output(response)).await {
            Ok(Ok(complete)) => complete,
            Ok(Err(error)) => {
                if let Some(audit) = audit {
                    audit.keep_failure(error.kind.status());
                }
                return error_response(protocol.shape, &error);
            }
            Err(_elapsed) => {
                edge.note_timeout();
                if let Some(audit) = audit {
                    audit.keep_failure(ErrorType::RateLimit.status());
                }
                return error_response(protocol.shape, &word_timeout(edge, &idle_timeout_error()));
            }
        };

    if complete.chunks.is_empty_output() {
        // A turn that produced nothing is a failed turn: billing it as a
        // completion would let a client treat silence as an answer. It is counted
        // as an upstream failure for the same reason: the answer arrived, but not
        // as an answer.
        edge.note_upstream_failure();
        if let Some(audit) = audit {
            audit.keep_response(ErrorType::RateLimit.status(), complete.raw.clone());
        }
        return error_response(protocol.shape, &empty_output_error());
    }

    // The turn finished, so whatever run of timeouts preceded it is over.
    edge.note_success();

    let meta = meta_for(adapter, request, None);
    let body = adapter.render_response(
        &meta,
        request,
        &complete.chunks.into_response(&meta.id, &meta.model, meta.created),
    );
    if let Some(audit) = audit {
        audit.keep_response(StatusCode::OK.as_u16(), complete.raw);
    }
    json_response(StatusCode::OK, &body)
}

/// A streamed answer.
async fn streaming(
    adapter: &dyn ProtocolAdapter,
    protocol: &Protocol,
    edge: &Arc<Edge>,
    request: &bifrost_core::CanonicalRequest,
    response: reqwest::Response,
    admission: Option<InflightPermit>,
    audit: Option<Audit>,
) -> Response {
    let model = model_for(adapter, request);
    let meta = meta_for(adapter, request, Some(model.clone()));
    let renderer = adapter.stream_renderer(&meta);

    let plan = StreamPlan {
        renderer,
        response,
        idle: edge.config().limits.idle_for(true),
        heartbeat: protocol.heartbeat.then(|| std::time::Duration::from_secs(15)),
        // The session keeps its own copy: a stream that never commits is dropped
        // inside `open`, and its record is written from here instead.
        capture: audit
            .clone()
            .map(|audit| Capture::new(Arc::clone(&audit.evidence), audit.turn, audit.request)),
        stall: edge.config().limits.client_stall().map(|after| Stall {
            after,
            edge: Arc::clone(edge),
        }),
    };

    match stream::Session::open(plan).await {
        Opening::Unwritten { error } => {
            // A turn that never became a stream: the session has already written its
            // capture under the status this error carries.
            if is_idle_timeout(&error) {
                edge.note_timeout();
            } else {
                // Nothing was written, so whatever went wrong is upstream of this
                // process: a refusal, a broken connection, or a line that was not
                // this protocol.
                edge.note_upstream_failure();
            }
            error_response(protocol.shape, &word_timeout(edge, &error))
        }
        Opening::Committed { frames, mut session } => {
            // The upstream did answer, so the turn before this one was not a
            // timeout — a stream that fails after committing is a different
            // failure, and the client is told about it in the stream's own words.
            edge.note_success();
            session.prime(&frames);
            let mut response = Response::new(stream::into_body(session, admission));
            *response.status_mut() = StatusCode::OK;
            *response.headers_mut() = stream::headers();
            response
        }
    }
}

/// Replace the generic timeout wording with this deployment's.
///
/// The message depends on how many timeouts have happened, which the edge
/// tracks and the error does not.
fn word_timeout(edge: &Edge, error: &Error) -> Error {
    if is_idle_timeout(error) {
        Error {
            message: edge.timeout_message(),
            ..error.clone()
        }
    } else {
        error.clone()
    }
}

fn admit(edge: &Arc<Edge>) -> Option<Option<InflightPermit>> {
    let ceiling = edge.config().limits.max_inflight;
    if ceiling == 0 {
        return Some(None);
    }
    edge.admit().map(Some)
}

fn model_for(adapter: &dyn ProtocolAdapter, request: &bifrost_core::CanonicalRequest) -> String {
    request
        .model
        .clone()
        .filter(|model| !model.is_empty())
        .unwrap_or_else(|| adapter.default_model().to_owned())
}

fn meta_for(
    adapter: &dyn ProtocolAdapter,
    request: &bifrost_core::CanonicalRequest,
    model: Option<String>,
) -> ResponseMeta {
    let now = now_unix() as i64;
    ResponseMeta {
        id: identifier(adapter.name()),
        model: model.unwrap_or_else(|| model_for(adapter, request)),
        created: now,
        completed: now,
    }
}

/// The id a response is delivered under.
///
/// The original mints one per response, and the prefix is the protocol's: a
/// client that sees `chatcmpl-` on an Anthropic response would be reading
/// another endpoint's conversation.
fn identifier(adapter: &str) -> String {
    let unique = bifrost_wire::uuid_v4(&bifrost_wire::SystemEntropy::new());
    let short = &unique[..12.min(unique.len())];
    match adapter {
        bifrost_protocol::openai::chat::NAME => format!("chatcmpl-{short}"),
        bifrost_protocol::anthropic::NAME => format!("msg_{short}"),
        _ => format!("resp_{unique}"),
    }
}

/// The model catalogue.
///
/// Which models to offer comes from [`crate::models`]: the upstream's own list
/// when it can be had, and a table compiled into this build when it cannot.
async fn models(State(edge): State<Arc<Edge>>, headers: HeaderMap) -> Response {
    let ids = crate::models::catalog(&edge, crate::auth::api_key(&headers).as_deref()).await;
    let now = now_unix();
    let data: Vec<serde_json::Value> = ids
        .iter()
        .map(|id| {
            json!({
                "id": id,
                "object": "model",
                "created": now,
                "owned_by": "command-code",
            })
        })
        .collect();
    json_response(StatusCode::OK, &json!({ "object": "list", "data": data }))
}

/// Why a body could not be read.
enum ReadError {
    /// The body exceeded the configured ceiling.
    TooLarge,
    /// The connection failed partway through.
    Unreadable,
}

/// Read a body, refusing anything over `limit` without buffering it.
///
/// The ceiling is enforced as bytes arrive rather than after the fact, so an
/// oversized body costs the limit and not the whole upload.
async fn read_body(body: Body, limit: u64) -> Result<Bytes, ReadError> {
    let mut stream = body.into_data_stream();
    let mut collected: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ReadError::Unreadable)?;
        if collected.len() as u64 + chunk.len() as u64 > limit {
            return Err(ReadError::TooLarge);
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(collected))
}

fn json_response(status: StatusCode, body: &serde_json::Value) -> Response {
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

fn error_response(shape: Shape, error: &Error) -> Response {
    let rendered = error::render(shape, error);
    let mut response = Response::new(Body::from(rendered.body.to_string()));
    *response.status_mut() = rendered.status;
    *response.headers_mut() = rendered.headers;
    response
}
