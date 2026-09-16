//! The HTTP surface.
//!
//! Routing, admission and the three protocol endpoints. The endpoints differ
//! only in which adapter they name and how they spell an error, so they run
//! through one driver rather than three copies of it.

use std::sync::Arc;

use crate::core::Error;
use crate::protocol::adapter::{ProtocolAdapter, ResponseMeta};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::StreamExt;
use serde_json::json;

use crate::edge::access::Caller;
use crate::edge::error::{self, Shape};
use crate::edge::lifecycle;
use crate::edge::log;
use crate::edge::state::{
    Edge, InflightPermit, Slots, empty_output_error, idle_timeout_error, is_idle_timeout, now_unix,
};
use crate::edge::stream::{self, Opening, Stall, StreamPlan};
use crate::edge::upstream;

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
    adapter: crate::protocol::openai::chat::NAME,
    shape: Shape::OpenAi,
    heartbeat: false,
};

const MESSAGES: Protocol = Protocol {
    adapter: crate::protocol::anthropic::NAME,
    shape: Shape::Anthropic,
    // The Anthropic SDKs time out on a missing first byte, and a reasoning model
    // can think for minutes before it produces one.
    heartbeat: true,
};

const RESPONSES: Protocol = Protocol {
    adapter: crate::protocol::openai::responses::NAME,
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
/// Read-only and aggregate: every number is about this process rather than about a
/// client's traffic, and none of them names a key, a model or a body. It answers the
/// question an operator asks a deployment that is refusing turns — is nothing
/// arriving, or is nothing working — without anyone reading the log. This is not the
/// metrics switch that was removed: there is no switch, there is nothing to
/// configure, and no decision anywhere is taken from a number in it.
///
/// One row per issued token does name callers, so where the deployment issues
/// tokens this page takes the same credential a turn does. A page that names people
/// is not one to hand to whatever can reach the port, and the port is the thing a
/// deployment on a server opens. A deployment that forwards its callers' keys has
/// no rows in it and goes on answering without one, and the probe is open either
/// way: it says one word about the process and nothing about who is using it.
async fn status(State(edge): State<Arc<Edge>>, headers: HeaderMap) -> Response {
    if let Some(access) = edge.access()
        && !access.holds(crate::edge::auth::credential(&headers))
    {
        edge.note_unauthenticated();
        return error_response(Shape::OpenAi, &Error::authentication(missing_credential(&edge)));
    }
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
            "tokens": status.tokens,
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

    // The refusal is a whole response, which is several times the size of the caller
    // that was being looked up, so it travels boxed rather than by value.
    let mut caller = match caller_for(&edge, protocol, &headers, &payload) {
        Ok(caller) => caller,
        Err(refusal) => return *refusal,
    };

    let decoded = match adapter.decode_request(&payload, &edge.convert_options()) {
        Ok(decoded) => decoded,
        Err(error) => {
            edge.note_malformed();
            return error_response(protocol.shape, &error.into_error());
        }
    };
    let mut request = decoded.value;
    detail.requested_model = apply_model_alias(edge.as_ref(), &mut request);
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

    detail.key_fingerprint = Some(crate::edge::auth::key_fingerprint(caller.key()));
    detail.token = caller.token().map(str::to_owned);
    let session = edge.session_for(caller.identity(), &headers, request.prompt_cache_key.as_deref());

    let context = edge.wire_context(caller.key(), &session);
    // The upstream is told which device this key is on, and that a session
    // exists, before it is asked to generate: the record is what makes the
    // identity observable, and the first turn of a window is the one that pays
    // for it. Nothing about it can fail the turn.
    lifecycle::announce(&edge, caller.key(), &context).await;

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
        return error_response(protocol.shape, &error);
    }

    let response = match upstream.into_result().await {
        Ok(response) => response,
        Err(error) => {
            edge.note_upstream_failure();
            return error_response(protocol.shape, &error);
        }
    };

    // Both places this turn holds travel together from here, because a stream
    // outlives the handler that opened it: the answer is still being written when
    // the request has already become a response, so what the turn is holding has to
    // be held by the stream.
    let slots = Slots {
        global: admission,
        token: caller.take_slot(),
    };

    if !request.stream {
        return complete(adapter, protocol, &edge, &request, response).await;
    }
    streaming(adapter, protocol, &edge, &request, response, slots).await
}

/// Work out who is asking, or the refusal to send back.
///
/// The credential is checked after the body, so a malformed request is reported as
/// malformed rather than as unauthenticated — a client sent to fix its credentials
/// would never learn that its JSON was broken.
fn caller_for(
    edge: &Arc<Edge>,
    protocol: &Protocol,
    headers: &HeaderMap,
    payload: &[u8],
) -> Result<Caller, Box<Response>> {
    let Some(presented) = crate::edge::auth::credential(headers) else {
        return Err(Box::new(unidentified(edge, protocol, payload)));
    };
    match edge.access() {
        // A deployment that issues tokens reads the credential whole rather than
        // scanning it for the shape of a key: what a caller carries is the token,
        // and a key that still worked would be a key no revocation reaches.
        Some(access) => match access.caller(presented) {
            Ok(caller) => Ok(caller),
            Err(error) => {
                edge.note_unauthenticated();
                Err(Box::new(error_response(protocol.shape, &error)))
            }
        },
        None => match crate::edge::auth::api_key(headers) {
            Some(key) => Ok(Caller::passthrough(key)),
            // A credential that is not one of this deployment's keys is refused
            // the same way as no credential at all, which is what the original
            // does: it scans for the shape of a key and finds none.
            None => Err(Box::new(unidentified(edge, protocol, payload))),
        },
    }
}

/// The refusal for a request that arrived without a usable credential.
///
/// The body is parsed here and only here, to choose between the two errors: when a
/// credential was recognised, the adapter parses the body anyway and reports the
/// same 400 for one that is not JSON, so the normal path pays nothing for this.
fn unidentified(edge: &Arc<Edge>, protocol: &Protocol, payload: &[u8]) -> Response {
    if serde_json::from_slice::<serde_json::Value>(payload).is_err() {
        edge.note_malformed();
        return error_response(protocol.shape, &Error::invalid_request("Invalid JSON body"));
    }
    edge.note_unauthenticated();
    error_response(protocol.shape, &Error::authentication(missing_credential(edge)))
}

/// The sentence a request with no usable credential is refused with.
///
/// One place because it is said on two surfaces — a turn, and the status page of a
/// deployment that names its callers — and the wording follows the two ways a
/// deployment can be wired: a token it issued, or the caller's own key.
fn missing_credential(edge: &Arc<Edge>) -> &'static str {
    if edge.access().is_some() {
        "Missing API key. Send the token this deployment issued you in Authorization: Bearer <token> or x-api-key header"
    } else {
        "Missing API key. Send in Authorization: Bearer <key> or x-api-key header"
    }
}

/// A non-streaming answer.
async fn complete(
    adapter: &dyn ProtocolAdapter,
    protocol: &Protocol,
    edge: &Edge,
    request: &crate::core::CanonicalRequest,
    response: reqwest::Response,
) -> Response {
    // The idle timeout covers the whole read rather than the gaps between reads.
    // There are no gaps to measure here: the upstream may answer in one piece, so
    // "no bytes for ninety seconds" and "no complete answer for ninety seconds"
    // are the same statement.
    let complete =
        match tokio::time::timeout(edge.config().limits.nonstream_idle(), upstream::read_output(response)).await {
            Ok(Ok(complete)) => complete,
            Ok(Err(error)) => return error_response(protocol.shape, &error),
            Err(_elapsed) => {
                edge.note_timeout();
                return error_response(protocol.shape, &word_timeout(edge, &idle_timeout_error()));
            }
        };

    if complete.chunks.is_empty_output() {
        // A turn that produced nothing is a failed turn: billing it as a
        // completion would let a client treat silence as an answer. It is counted
        // as an upstream failure for the same reason: the answer arrived, but not
        // as an answer.
        edge.note_upstream_failure();
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
    json_response(StatusCode::OK, &body)
}

/// A streamed answer.
async fn streaming(
    adapter: &dyn ProtocolAdapter,
    protocol: &Protocol,
    edge: &Arc<Edge>,
    request: &crate::core::CanonicalRequest,
    response: reqwest::Response,
    slots: Slots,
) -> Response {
    let model = model_for(adapter, request);
    let meta = meta_for(adapter, request, Some(model.clone()));
    let renderer = adapter.stream_renderer(&meta);

    let plan = StreamPlan {
        renderer,
        response,
        idle: edge.config().limits.idle_for(true),
        heartbeat: protocol.heartbeat.then(|| std::time::Duration::from_secs(15)),
        stall: edge.config().limits.client_stall().map(|after| Stall {
            after,
            edge: Arc::clone(edge),
        }),
    };

    match stream::Session::open(plan).await {
        Opening::Unwritten { error } => {
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
            let mut response = Response::new(stream::into_body(session, slots));
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

/// Point a request at the model this deployment's plan serves, when a rule says so.
///
/// Returns the name the client asked for when a rule rewrote it, so that the turn
/// can record both what was asked and what was used; `None` means the request goes
/// out under the name it arrived with, which is the case for every unmapped name.
///
/// Rewriting the request rather than the envelope is what keeps the answer honest:
/// the upstream is asked for one model and the response echoes that same model, so
/// a client that reads back `model` is reading the name its answer came from.
fn apply_model_alias(edge: &Edge, request: &mut crate::core::CanonicalRequest) -> Option<String> {
    let requested = request.model.clone()?;
    let alias = edge.config().models.resolve(&requested)?;
    if alias == requested {
        return None;
    }
    request.model = Some(alias.to_owned());
    Some(requested)
}

fn admit(edge: &Arc<Edge>) -> Option<Option<InflightPermit>> {
    let ceiling = edge.config().limits.max_inflight;
    if ceiling == 0 {
        return Some(None);
    }
    edge.admit().map(Some)
}

fn model_for(adapter: &dyn ProtocolAdapter, request: &crate::core::CanonicalRequest) -> String {
    request
        .model
        .clone()
        .filter(|model| !model.is_empty())
        .unwrap_or_else(|| adapter.default_model().to_owned())
}

fn meta_for(
    adapter: &dyn ProtocolAdapter,
    request: &crate::core::CanonicalRequest,
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
    let unique = crate::wire::uuid_v4(&crate::wire::SystemEntropy::new());
    let short = &unique[..12.min(unique.len())];
    match adapter {
        crate::protocol::openai::chat::NAME => format!("chatcmpl-{short}"),
        crate::protocol::anthropic::NAME => format!("msg_{short}"),
        _ => format!("resp_{unique}"),
    }
}

/// The model catalogue.
///
/// Which models to offer comes from [`crate::edge::models`]: the upstream's own list
/// when it can be had, and a table compiled into this build when it cannot.
async fn models(State(edge): State<Arc<Edge>>, headers: HeaderMap) -> Response {
    // A catalogue is the deployment's own question rather than a caller's, so a
    // deployment that issues tokens asks it with the key it holds: an anonymous
    // `GET /v1/models` names the models this account has, which is what the endpoint
    // is for, and the fetch behind it is cached and shared, so it costs one call
    // between every caller rather than one each.
    let key = match edge.access() {
        Some(access) => Some(access.key().to_owned()),
        None => crate::edge::auth::api_key(&headers),
    };
    let ids = crate::edge::models::catalog(&edge, key.as_deref()).await;
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
