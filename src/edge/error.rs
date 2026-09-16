//! Rendering the shared error surface into each protocol's spelling.
//!
//! Classification happens once, in `crate::core`; this module only decides how
//! it is written down. The three endpoints do not agree on that writing in the
//! original: Chat answers `auth_error` where the other two answer
//! `authentication_error`, and its mid-stream failures answer `proxy_error`
//! where its own non-streaming path answers `upstream_error`, with an
//! `input_tokens` field that appears nowhere else.
//!
//! Reproducing that would mean three error vocabularies for one classification,
//! which is the thing the shared surface exists to prevent, so the spelling here
//! is uniform. It is the one place the edge deliberately differs from the
//! original, and the tests at the bottom pin the difference.

use crate::core::Error;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use serde_json::{Map, Value, json};

/// Which protocol is answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// OpenAI Chat Completions: a bare `error` object.
    OpenAi,
    /// Anthropic Messages: a named envelope around the same object.
    Anthropic,
    /// OpenAI Responses: like Chat, but with the two fields the endpoint
    /// documents always present, even when they carry nothing.
    Responses,
}

/// A rendered failure: what to answer, with which headers and body.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Value,
}

/// Spell one classified failure for one protocol.
#[must_use]
pub fn render(shape: Shape, error: &Error) -> Rendered {
    let detail = error.body().error;
    let inner = |code: Option<&str>| {
        let mut object = Map::new();
        object.insert("message".to_owned(), json!(detail.message));
        object.insert("type".to_owned(), json!(detail.kind));
        if let Some(code) = code {
            object.insert("code".to_owned(), json!(code));
        }
        object
    };

    let mut body = match shape {
        // The original omits `code` entirely when the upstream named none, and
        // never sends `param`.
        Shape::OpenAi => json!({ "error": Value::Object(inner(detail.code.as_deref())) }),
        Shape::Anthropic => {
            let mut inner = inner(None);
            inner.remove("code");
            json!({ "type": "error", "error": Value::Object(inner) })
        }
        // Documented as always present, so an absent one is spelled `null`
        // rather than dropped. The key order is the original's.
        Shape::Responses => json!({
            "error": {
                "message": detail.message,
                "type": detail.kind,
                "code": detail.code,
                "param": detail.param,
            }
        }),
    };

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(retry_after) = error.retry_after {
        // Both spellings: the hint belongs in the body because the original puts
        // it there, and in the header because SDKs back off on that alone.
        if let Some(object) = body.as_object_mut() {
            object.insert("retry_after".to_owned(), json!(retry_after));
        }
        if let Ok(value) = HeaderValue::from_str(&retry_after.to_string()) {
            headers.insert(header::RETRY_AFTER, value);
        }
    }

    Rendered {
        status: StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        headers,
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ErrorType;

    #[test]
    fn chat_drops_fields_it_has_no_value_for() {
        let rendered = render(Shape::OpenAi, &Error::invalid_request("bad shape"));
        assert_eq!(
            serde_json::to_string(&rendered.body).expect("serialize"),
            r#"{"error":{"message":"bad shape","type":"invalid_request_error"}}"#
        );
        assert_eq!(rendered.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn chat_forwards_the_upstream_code_it_was_given() {
        let error = Error::from_upstream(402, Some("USAGE_EXCEEDED".to_owned()), "out of quota");
        let rendered = render(Shape::OpenAi, &error);
        assert_eq!(
            serde_json::to_string(&rendered.body).expect("serialize"),
            r#"{"error":{"message":"out of quota","type":"rate_limit_error","code":"USAGE_EXCEEDED"},"retry_after":30}"#
        );
    }

    #[test]
    fn anthropic_wraps_the_same_object_in_a_named_envelope() {
        let rendered = render(Shape::Anthropic, &Error::invalid_request("bad shape"));
        assert_eq!(
            serde_json::to_string(&rendered.body).expect("serialize"),
            r#"{"type":"error","error":{"message":"bad shape","type":"invalid_request_error"}}"#
        );
    }

    #[test]
    fn responses_spells_the_absent_fields_as_null() {
        let rendered = render(Shape::Responses, &Error::invalid_request("bad shape"));
        assert_eq!(
            serde_json::to_string(&rendered.body).expect("serialize"),
            r#"{"error":{"message":"bad shape","type":"invalid_request_error","code":null,"param":null}}"#
        );
    }

    #[test]
    fn a_throttle_carries_its_hint_in_both_spellings() {
        for shape in [Shape::OpenAi, Shape::Anthropic, Shape::Responses] {
            let rendered = render(shape, &Error::rate_limit("slow down"));
            assert_eq!(rendered.status, StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(rendered.headers.get(header::RETRY_AFTER).expect("header"), "30");
            assert_eq!(rendered.body["retry_after"], json!(30));
            assert_eq!(
                rendered.body["error"]["type"],
                json!(ErrorType::RateLimit.wire_type()),
                "every protocol classifies a throttle the same way"
            );
        }
    }

    #[test]
    fn an_unhinted_failure_has_no_retry_header() {
        let rendered = render(Shape::OpenAi, &Error::invalid_request("bad shape"));
        assert!(rendered.headers.get(header::RETRY_AFTER).is_none());
        assert!(rendered.body.get("retry_after").is_none());
    }

    #[test]
    fn a_too_large_body_keeps_its_status_and_its_classification() {
        let rendered = render(
            Shape::OpenAi,
            &Error::payload_too_large("Request body exceeds 100MB limit"),
        );
        assert_eq!(rendered.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(rendered.body["error"]["type"], json!("invalid_request_error"));
    }
}
