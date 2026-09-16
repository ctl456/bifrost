//! Calling the upstream and reading what it answers.

use crate::core::Error;
use serde_json::Value;

use crate::edge::state::Edge;

/// An upstream response, body still unread.
pub struct Upstream {
    pub status: u16,
    response: reqwest::Response,
}

impl Upstream {
    /// Whether the upstream accepted the request.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Read the whole body and classify a failure.
    ///
    /// The upstream reports a rejected request as an HTTP status *and* a JSON
    /// body naming the reason, so the status alone is not enough to tell a
    /// client what to fix.
    pub async fn into_result(self) -> Result<reqwest::Response, Error> {
        if self.is_success() {
            return Ok(self.response);
        }
        let status = self.status;
        let text = self.response.text().await.unwrap_or_default();
        Err(classify(status, &text))
    }

    /// The raw body, whether or not the status was a success.
    pub async fn text(self) -> String {
        self.response.text().await.unwrap_or_default()
    }
}

/// Send an encoded request upstream.
pub async fn send(edge: &Edge, request: &crate::wire::WireRequest) -> Result<Upstream, Error> {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

    let mut headers = HeaderMap::new();
    for (name, value) in &request.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| Error::internal(format!("bad upstream header name `{name}`: {error}")))?;
        let value = HeaderValue::from_str(value)
            .map_err(|error| Error::internal(format!("bad upstream header value for `{name}`: {error}")))?;
        headers.insert(name, value);
    }

    let mut outbound = edge
        .client()
        .request(
            reqwest::Method::from_bytes(request.method.as_bytes())
                .map_err(|error| Error::internal(format!("bad method `{}`: {error}", request.method)))?,
            request.url(&edge.config().api_base),
        )
        .headers(headers);
    // Absent when the dialect said there is none, rather than an empty body.
    if let Some(body) = request.body_string() {
        outbound = outbound.body(body);
    }

    let response = outbound
        .send()
        .await
        .map_err(|error| Error::upstream(format!("Upstream error: {error}")))?;

    let status = response.status().as_u16();
    Ok(Upstream { status, response })
}

/// Turn a non-2xx upstream answer into the shared error surface.
///
/// The body is JSON in the normal case, but a gateway in front of the upstream
/// can answer with HTML or nothing at all, so a parse failure degrades to the
/// first bytes of whatever arrived rather than to a message that names nothing.
#[must_use]
pub fn classify(status: u16, body: &str) -> Error {
    let (message, code) = describe(body);
    let message = if message.is_empty() {
        format!("CC API error ({status})")
    } else {
        message
    };
    Error::from_upstream(status, code, message)
}

/// Pull the message and machine-readable code out of an upstream error body.
fn describe(body: &str) -> (String, Option<String>) {
    let Ok(parsed) = serde_json::from_str::<Value>(body) else {
        return (truncate(body, 200), None);
    };
    let message = parsed
        .get("error")
        .and_then(|error| error.get("message"))
        .or_else(|| parsed.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    // The upstream's own classification (`BAD_REQUEST`, `USAGE_EXCEEDED`), kept
    // verbatim so a client can branch on it.
    let code = parsed
        .get("error")
        .and_then(|error| error.get("code"))
        .or_else(|| parsed.get("code"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    (message, code)
}

fn truncate(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Split a stream of bytes into lines.
///
/// The upstream speaks newline-delimited JSON, and a read can end anywhere — in
/// the middle of a line, or of a multi-byte character — so the tail is carried
/// to the next read instead of being decoded as if it were whole.
#[derive(Debug, Default)]
pub struct LineBuffer {
    pending: Vec<u8>,
}

impl LineBuffer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a read and take the complete lines it completed.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(chunk);
        let mut lines = Vec::new();
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            lines.push(String::from_utf8_lossy(&line[..line.len() - 1]).into_owned());
        }
        lines
    }

    /// How much of a line is still waiting for its terminator.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Whatever is left, for the end of a stream that did not end on a newline.
    pub fn take_remainder(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }
        let tail = std::mem::take(&mut self.pending);
        Some(String::from_utf8_lossy(&tail).into_owned())
    }
}

/// A complete upstream answer: what it decoded to.
pub struct Complete {
    pub chunks: crate::core::OutputAccumulator,
}

/// Read a complete (non-streaming) answer into canonical chunks.
pub async fn read_output(response: reqwest::Response) -> Result<Complete, Error> {
    let raw = response
        .bytes()
        .await
        .map_err(|error| Error::upstream(format!("Upstream error: {error}")))?;
    let mut lines = LineBuffer::new();
    let mut decoder = crate::wire::cc::CcDecoder::new();
    let mut chunks = crate::core::OutputAccumulator::new();
    for line in lines.push(&raw) {
        chunks.extend(&decoder.decode_line(&line).chunks);
    }
    if let Some(tail) = lines.take_remainder() {
        chunks.extend(&decoder.decode_line(&tail).chunks);
    }
    if let Some(error) = chunks.error.clone() {
        return Err(error);
    }
    Ok(Complete { chunks })
}
