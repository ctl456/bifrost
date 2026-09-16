//! Server-sent-event framing.
//!
//! Framing is protocol-independent, so it lives in one place: a protocol adapter
//! decides *what* to send, never how a frame is spelled.

use std::fmt;

use serde_json::Value;

/// One complete SSE frame, terminator included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame(String);

impl SseFrame {
    /// `data: <json>\n\n`
    #[must_use]
    pub fn data(payload: &Value) -> Self {
        Self(format!("data: {payload}\n\n"))
    }

    /// `event: <name>\ndata: <json>\n\n`
    ///
    /// Anthropic's endpoints name every event; OpenAI's do not.
    #[must_use]
    pub fn named(event: &str, payload: &Value) -> Self {
        Self(format!("event: {event}\ndata: {payload}\n\n"))
    }

    /// A frame whose bytes are already framed.
    ///
    /// Only for the fixed sentinels below; anything dynamic goes through
    /// [`SseFrame::data`] so the framing cannot be forgotten.
    #[must_use]
    pub fn from_raw(raw: &str) -> Self {
        Self(raw.to_owned())
    }

    /// `: <text>\n\n`, which clients ignore.
    ///
    /// Used to hold a connection open while the upstream is thinking, so a
    /// client-side idle timeout does not fire on a healthy request.
    #[must_use]
    pub fn comment(text: &str) -> Self {
        Self(format!(": {text}\n\n"))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SseFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl AsRef<str> for SseFrame {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// The sentinel every OpenAI-compatible stream ends with.
pub const DONE: &str = "data: [DONE]\n\n";

/// The keepalive the original proxy emits on an otherwise silent stream.
pub const KEEPALIVE: &str = ": keepalive\n\n";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_data_frame_is_terminated_by_a_blank_line() {
        let frame = SseFrame::data(&json!({ "a": 1 }));
        assert_eq!(frame.as_str(), "data: {\"a\":1}\n\n");
        assert_eq!(frame.to_string(), frame.as_str());
        assert_eq!(frame.as_ref(), "data: {\"a\":1}\n\n");
    }

    #[test]
    fn a_named_frame_prefixes_the_event() {
        assert_eq!(
            SseFrame::named("content_block_delta", &json!({ "i": 0 })).as_str(),
            "event: content_block_delta\ndata: {\"i\":0}\n\n"
        );
    }

    #[test]
    fn a_raw_frame_is_taken_verbatim() {
        assert_eq!(SseFrame::from_raw(DONE).as_str(), DONE);
    }

    #[test]
    fn comments_are_frames_too() {
        assert_eq!(SseFrame::comment("ping").as_str(), ": ping\n\n");
        assert_eq!(KEEPALIVE, ": keepalive\n\n");
    }
}
