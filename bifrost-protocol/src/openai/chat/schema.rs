//! The OpenAI Chat Completions wire types.
//!
//! These describe what a *client* sends. Nothing here is shared with the
//! outbound side: the response shape is built by hand so its key order is
//! explicit, and a field only needs a response type if some client reads it.

use serde::{Deserialize, Serialize};

use crate::openai::shared::{FunctionTool, OpenAiToolChoice};

/// A `/v1/chat/completions` request body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
    /// Accepted as an alias for `max_tokens`.
    ///
    /// Both spellings are in active use and mean the same thing here; rejecting
    /// the newer one would break clients for no benefit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopSequence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<FunctionTool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<OpenAiToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Names the prompt-cache bucket the client wants.
    ///
    /// Also used as a session hint, but that is the session mechanism's
    /// decision, not this schema's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    /// Attributed end user; see `CanonicalRequest::user`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

/// `stop` is a string or an array of strings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StopSequence {
    One(String),
    Many(Vec<String>),
}

impl StopSequence {
    #[must_use]
    pub fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(one) => vec![one],
            Self::Many(many) => many,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,
    /// Assistant turn metadata.
    ///
    /// The upstream validates that thinking is sent back with the history, so a
    /// client that carries `reasoning_content` must have it forwarded verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentPart {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// A second spelling of [`Self::text`] some wrappers emit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<ImageUrl>,
    /// The client's cache breakpoint, in the `{"type":"ephemeral"}` shape.
    ///
    /// Typed loosely because it is forwarded verbatim rather than interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<serde_json::Value>,
    /// Every other key the client sent alongside the part.
    ///
    /// A part this build does not model still has to reach the wire unchanged,
    /// and re-serializing the fields above would reorder its keys and drop the
    /// ones not named here.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl ContentPart {
    /// The part as the client wrote it, with the tag back in front.
    ///
    /// Rebuilt rather than captured because the tag is the one key the typed
    /// view consumes; the rest are still in the order they arrived. A client
    /// that wrote `type` last sees it move to the front, which is the only way
    /// this differs from the bytes on the socket.
    #[must_use]
    pub fn verbatim(&self) -> serde_json::Value {
        let mut object = serde_json::Map::with_capacity(self.extra.len() + 1);
        object.insert("type".to_owned(), serde_json::Value::String(self.kind.clone()));
        object.extend(self.extra.iter().map(|(key, value)| (key.clone(), value.clone())));
        serde_json::Value::Object(object)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    /// Accepted and ignored, as the original proxy does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ImageUrl {
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            detail: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// `call_...`, in practice.
    ///
    /// Optional because a client resuming a trimmed history can send a call
    /// without one, and refusing the whole request over that would be worse than
    /// synthesizing an id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    pub function: ToolCallFunction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallFunction {
    #[serde(default)]
    pub name: String,
    /// Arguments as a JSON string.
    ///
    /// Some clients send an object instead; that is accepted at the boundary and
    /// normalized, because the upstream wants a parsed value either way.
    #[serde(default)]
    pub arguments: ArgumentPayload,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ArgumentPayload {
    #[default]
    Absent,
    Text(String),
    Structured(serde_json::Value),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stop_accepts_one_or_many() {
        let one: ChatCompletionRequest = serde_json::from_value(json!({ "stop": "\n" })).expect("parse");
        assert_eq!(one.stop.expect("stop").into_vec(), vec!["\n"]);

        let many: ChatCompletionRequest = serde_json::from_value(json!({ "stop": ["a", "b"] })).expect("parse");
        assert_eq!(many.stop.expect("stop").into_vec(), vec!["a", "b"]);
    }

    #[test]
    fn content_accepts_a_string_or_parts() {
        let text: ChatMessage = serde_json::from_value(json!({ "role": "user", "content": "hi" })).expect("parse");
        assert_eq!(text.content, Some(MessageContent::Text("hi".to_owned())));

        let parts: ChatMessage = serde_json::from_value(json!({
            "role": "user",
            "content": [{ "type": "text", "text": "hi" }]
        }))
        .expect("parse");
        let Some(MessageContent::Parts(parts)) = parts.content else {
            panic!("expected parts");
        };
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].kind, "text");
    }

    #[test]
    fn tool_arguments_accept_text_or_a_structure() {
        let text: ToolCallFunction = serde_json::from_value(json!({ "name": "f", "arguments": "{}" })).expect("parse");
        assert_eq!(text.arguments, ArgumentPayload::Text("{}".to_owned()));

        let structured: ToolCallFunction =
            serde_json::from_value(json!({ "name": "f", "arguments": { "a": 1 } })).expect("parse");
        assert_eq!(structured.arguments, ArgumentPayload::Structured(json!({ "a": 1 })));

        let absent: ToolCallFunction = serde_json::from_value(json!({ "name": "f" })).expect("parse");
        assert_eq!(absent.arguments, ArgumentPayload::Absent);
    }

    #[test]
    fn a_request_tolerates_unknown_fields() {
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "m",
            "messages": [],
            "logit_bias": { "1": 2 },
            "user": "u"
        }))
        .expect("parse");
        assert_eq!(request.model.as_deref(), Some("m"));
    }

    #[test]
    fn max_tokens_accepts_both_spellings() {
        let request: ChatCompletionRequest = serde_json::from_value(json!({
            "messages": [],
            "max_completion_tokens": 128
        }))
        .expect("parse");
        assert_eq!(request.max_tokens, None);
        assert_eq!(request.max_completion_tokens, Some(128));
    }
}
