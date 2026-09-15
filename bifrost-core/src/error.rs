//! The single error surface shared by all adapters.
//!
//! Upstream status codes are translated once, here, so every protocol renders
//! the same classification. Protocol crates only decide how to spell it on the
//! wire.

use serde::{Deserialize, Serialize};

/// Retry hint attached to every throttle response.
pub const DEFAULT_RETRY_AFTER_SECS: u32 = 30;

/// How an error is classified, independent of transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorType {
    InvalidRequest,
    Authentication,
    NotFound,
    RateLimit,
    Upstream,
    Unavailable,
    Internal,
}

impl ErrorType {
    /// The `type` string OpenAI-compatible clients expect.
    #[must_use]
    pub const fn wire_type(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request_error",
            Self::Authentication => "authentication_error",
            Self::NotFound => "not_found",
            Self::RateLimit => "rate_limit_error",
            Self::Upstream => "upstream_error",
            Self::Unavailable => "temporarily_unavailable",
            Self::Internal => "internal_error",
        }
    }

    /// The HTTP status Bifrost emits for this classification.
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            Self::InvalidRequest => 400,
            Self::Authentication => 401,
            Self::NotFound => 404,
            Self::RateLimit => 429,
            Self::Upstream => 502,
            Self::Unavailable => 503,
            Self::Internal => 500,
        }
    }
}

/// Translate an upstream status into the status and classification Bifrost
/// reports downstream.
///
/// The upstream is not OpenAI-shaped: it answers `402 Payment Required` for an
/// exhausted quota and `422` for a malformed body. Both are remapped so client
/// SDKs behave (402 becomes a retryable throttle, 422 a plain bad request).
#[must_use]
pub const fn map_upstream_status(status: u16) -> (u16, ErrorType) {
    match status {
        400 => (400, ErrorType::InvalidRequest),
        401 => (401, ErrorType::Authentication),
        402 => (429, ErrorType::RateLimit),
        403 => (401, ErrorType::Authentication),
        404 => (404, ErrorType::NotFound),
        422 => (400, ErrorType::InvalidRequest),
        429 => (429, ErrorType::RateLimit),
        500 | 502 => (502, ErrorType::Upstream),
        503 => (503, ErrorType::Unavailable),
        _ => (502, ErrorType::Upstream),
    }
}

/// A classification plus the context needed to render it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    pub kind: ErrorType,
    /// The HTTP status Bifrost returns to the client.
    pub status: u16,
    pub message: String,
    /// The upstream's machine-readable code (`USAGE_EXCEEDED`, `BAD_REQUEST`, …),
    /// forwarded so operators and SDKs can branch on it.
    pub upstream_code: Option<String>,
    pub retry_after: Option<u32>,
}

impl Error {
    fn new(kind: ErrorType, message: impl Into<String>) -> Self {
        Self {
            kind,
            status: kind.status(),
            message: message.into(),
            upstream_code: None,
            retry_after: None,
        }
    }

    #[must_use]
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(ErrorType::InvalidRequest, message)
    }

    #[must_use]
    pub fn authentication(message: impl Into<String>) -> Self {
        Self::new(ErrorType::Authentication, message)
    }

    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorType::NotFound, message)
    }

    #[must_use]
    pub fn upstream(message: impl Into<String>) -> Self {
        Self::new(ErrorType::Upstream, message)
    }

    #[must_use]
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorType::Unavailable, message)
    }

    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorType::Internal, message)
    }

    /// A throttle response carrying a retry hint.
    #[must_use]
    pub fn rate_limit(message: impl Into<String>) -> Self {
        Self {
            retry_after: Some(DEFAULT_RETRY_AFTER_SECS),
            ..Self::new(ErrorType::RateLimit, message)
        }
    }

    /// The request body exceeded the configured ceiling.
    #[must_use]
    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self {
            status: 413,
            ..Self::new(ErrorType::InvalidRequest, message)
        }
    }

    /// Classify a raw upstream failure.
    ///
    /// `upstream_code` is the upstream's own code, preserved verbatim when
    /// present. A throttle classification always carries a retry hint so client
    /// SDKs back off instead of hammering.
    #[must_use]
    pub fn from_upstream(status: u16, upstream_code: Option<String>, message: impl Into<String>) -> Self {
        let (mapped_status, kind) = map_upstream_status(status);
        let retry_after = (kind == ErrorType::RateLimit).then_some(DEFAULT_RETRY_AFTER_SECS);
        Self {
            kind,
            status: mapped_status,
            message: message.into(),
            upstream_code,
            retry_after,
        }
    }

    /// Whether a client is expected to retry this request as-is.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            ErrorType::RateLimit | ErrorType::Unavailable | ErrorType::Upstream
        )
    }

    /// The shared JSON body shape used by the OpenAI-compatible endpoints.
    #[must_use]
    pub fn body(&self) -> ErrorBody {
        ErrorBody {
            error: ErrorDetail {
                message: self.message.clone(),
                kind: self.kind.wire_type().to_owned(),
                param: None,
                code: self
                    .upstream_code
                    .clone()
                    .or_else(|| (self.status >= 500).then(|| self.kind.wire_type().to_owned())),
            },
        }
    }
}

/// An `error` object as returned by the OpenAI-compatible endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorDetail {
    pub message: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// A failure while converting a protocol payload into the canonical form.
///
/// Adapters report *why* the client's input was rejected; the caller decides
/// how to render it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConversionError {
    #[error("invalid request: {detail}")]
    BadRequest { detail: String, param: Option<String> },
    #[error("internal error: {detail}")]
    Internal { detail: String },
}

impl ConversionError {
    #[must_use]
    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::BadRequest {
            detail: detail.into(),
            param: None,
        }
    }

    #[must_use]
    pub fn bad_request_at(detail: impl Into<String>, param: impl Into<String>) -> Self {
        Self::BadRequest {
            detail: detail.into(),
            param: Some(param.into()),
        }
    }

    #[must_use]
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::Internal { detail: detail.into() }
    }

    #[must_use]
    pub const fn status_code(&self) -> u16 {
        match self {
            Self::BadRequest { .. } => 400,
            Self::Internal { .. } => 500,
        }
    }

    /// Promote into the shared error surface.
    #[must_use]
    pub fn into_error(self) -> Error {
        match self {
            Self::BadRequest { detail, param } => {
                let mut error = Error::invalid_request(detail);
                if param.is_some() {
                    error.upstream_code = None;
                }
                error
            }
            Self::Internal { detail } => Error::internal(detail),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn upstream_status_table_matches_the_proxy() {
        assert_eq!(map_upstream_status(400), (400, ErrorType::InvalidRequest));
        assert_eq!(map_upstream_status(401), (401, ErrorType::Authentication));
        assert_eq!(map_upstream_status(403), (401, ErrorType::Authentication));
        assert_eq!(map_upstream_status(404), (404, ErrorType::NotFound));
        assert_eq!(map_upstream_status(422), (400, ErrorType::InvalidRequest));
        assert_eq!(map_upstream_status(500), (502, ErrorType::Upstream));
        assert_eq!(map_upstream_status(502), (502, ErrorType::Upstream));
        assert_eq!(map_upstream_status(503), (503, ErrorType::Unavailable));
    }

    #[test]
    fn exhausted_quota_becomes_a_retryable_throttle() {
        assert_eq!(map_upstream_status(402), (429, ErrorType::RateLimit));
        let error = Error::from_upstream(402, Some("USAGE_EXCEEDED".to_owned()), "out of quota");
        assert_eq!(error.status, 429);
        assert_eq!(error.retry_after, Some(DEFAULT_RETRY_AFTER_SECS));
        assert!(error.is_retryable());
        assert_eq!(error.upstream_code.as_deref(), Some("USAGE_EXCEEDED"));
    }

    #[test]
    fn throttles_always_carry_a_retry_hint() {
        let error = Error::from_upstream(429, None, "slow down");
        assert_eq!(error.retry_after, Some(DEFAULT_RETRY_AFTER_SECS));
    }

    #[test]
    fn unknown_statuses_degrade_to_upstream_error() {
        let error = Error::from_upstream(418, None, "teapot");
        assert_eq!(error.status, 502);
        assert_eq!(error.kind, ErrorType::Upstream);
        assert_eq!(error.retry_after, None);
    }

    #[test]
    fn server_errors_echo_their_type_as_the_code() {
        let error = Error::from_upstream(500, None, "boom");
        assert_eq!(error.body().error.code.as_deref(), Some("upstream_error"));
    }

    #[test]
    fn client_errors_do_not_invent_a_code() {
        let error = Error::invalid_request("bad shape");
        assert_eq!(error.body().error.code, None);
        assert_eq!(
            serde_json::to_value(error.body()).expect("serialize"),
            json!({ "error": { "message": "bad shape", "type": "invalid_request_error" } })
        );
    }

    #[test]
    fn payload_too_large_reports_413() {
        let error = Error::payload_too_large("Request body exceeds 100MB limit");
        assert_eq!(error.status, 413);
        assert!(!error.is_retryable());
    }

    #[test]
    fn conversion_errors_map_onto_the_shared_surface() {
        let error = ConversionError::bad_request_at("missing field", "messages").into_error();
        assert_eq!(error.status, 400);
        assert_eq!(error.kind, ErrorType::InvalidRequest);
        assert_eq!(ConversionError::bad_request("x").status_code(), 400);
        assert_eq!(ConversionError::internal("y").status_code(), 500);
    }
}
