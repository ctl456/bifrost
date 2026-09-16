//! Reading the client's API key out of its headers.
//!
//! Both SDK styles are accepted because the same proxy serves both: the OpenAI
//! SDKs send `Authorization: Bearer …` and the Anthropic ones send a bare
//! `x-api-key`. In each case the key is the `user_…` token *inside* the header,
//! not the whole value — the original scans for that shape rather than trusting
//! the value, so a key wrapped in extra text still resolves.

use axum::http::HeaderMap;

/// The prefix every key carries.
const KEY_PREFIX: &str = "user_";

/// The credential a client sent, whole, in whichever of the two styles.
///
/// [`api_key`] scans a credential for the shape of a key, which is what
/// forwarding a client's own key needs. A deployment that issues tokens needs the
/// other thing: the value the client sent, compared against what it issued.
#[must_use]
pub fn credential(headers: &HeaderMap) -> Option<&str> {
    bearer(headers).or_else(|| x_api_key(headers))
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

fn x_api_key(headers: &HeaderMap) -> Option<&str> {
    headers.get("x-api-key").and_then(|value| value.to_str().ok())
}

/// Extract the API key, if the client sent one that is shaped like a key.
#[must_use]
pub fn api_key(headers: &HeaderMap) -> Option<String> {
    if let Some(rest) = bearer(headers)
        && let Some(key) = scan(rest)
    {
        return Some(key);
    }
    x_api_key(headers).and_then(scan)
}

/// The first `user_…` token in `text`.
///
/// The character class is the original's: anything alphanumeric, plus `_` and
/// `-`. A `user_` with nothing after it is not a key.
fn scan(text: &str) -> Option<String> {
    let start = text.find(KEY_PREFIX)?;
    let tail = &text[start..];
    let end = tail
        .char_indices()
        .take_while(|(index, character)| {
            *index < KEY_PREFIX.len() || character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
        })
        .last()
        .map(|(index, character)| index + character.len_utf8())?;
    (end > KEY_PREFIX.len()).then(|| tail[..end].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                HeaderValue::from_str(value).expect("header value"),
            );
        }
        headers
    }

    #[test]
    fn the_openai_style_header_is_read() {
        let key = api_key(&headers(&[("authorization", "Bearer user_abc-123_X")]));
        assert_eq!(key.as_deref(), Some("user_abc-123_X"));
    }

    #[test]
    fn the_anthropic_style_header_is_read() {
        let key = api_key(&headers(&[("x-api-key", "user_abc123")]));
        assert_eq!(key.as_deref(), Some("user_abc123"));
    }

    #[test]
    fn the_key_is_found_inside_a_wrapped_value() {
        let key = api_key(&headers(&[("x-api-key", "prefix user_abc123 suffix")]));
        assert_eq!(key.as_deref(), Some("user_abc123"));
    }

    #[test]
    fn a_bearer_header_without_a_key_falls_through_to_the_other_header() {
        let key = api_key(&headers(&[
            ("authorization", "Bearer sk-not-ours"),
            ("x-api-key", "user_abc123"),
        ]));
        assert_eq!(key.as_deref(), Some("user_abc123"));
    }

    #[test]
    fn a_bare_prefix_is_not_a_key() {
        assert!(api_key(&headers(&[("x-api-key", "user_")])).is_none());
        assert!(api_key(&headers(&[("x-api-key", "user_ ")])).is_none());
    }

    #[test]
    fn a_bearer_less_authorization_header_is_not_consulted() {
        // The original only looks inside `authorization` when it starts with
        // `Bearer `, so a bare key sent there is ignored rather than picked up
        // by the scan. That is a client's mistake either way, and refusing it
        // matches the original instead of answering a request it would reject.
        assert!(api_key(&headers(&[("authorization", "user_abc123")])).is_none());
    }

    #[test]
    fn an_absent_header_yields_nothing() {
        assert!(api_key(&HeaderMap::new()).is_none());
    }
}
