//! The Anthropic Messages wire types.
//!
//! These describe what a *client* sends. Nothing is `deny_unknown_fields`: this
//! endpoint is spoken by SDKs that grow fields faster than a gateway can track
//! them, and refusing a request over a key Bifrost does not read would break
//! clients that are otherwise perfectly compatible.
//!
//! Blocks are modelled as one permissive struct rather than a tagged enum. A
//! client that sends a block type this build has never heard of should lose that
//! block and keep its answer, and a tagged enum would reject the whole request
//! instead.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A `/v1/messages` request body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessagesRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// A prompt string, or sections of one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemPrompt>,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

/// `system` as a string or as sections.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SystemPrompt {
    Text(String),
    Sections(Vec<Block>),
    /// Everything else, so an unexpected shape costs a warning instead of the
    /// whole request.
    Other(Value),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<Block>),
    Other(Value),
}

/// One content block, from either direction.
///
/// One struct for every block type: the fields are disjoint in practice, and a
/// per-type enum would have to be extended in lockstep with the provider's
/// releases to keep parsing at all.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Block {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// A `thinking` block's content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ImageSource>,
    /// A `tool_use` block's call id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    /// A `tool_result` block's reference to the call it answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// A `tool_result` block's output: a string, text blocks, or a bare value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    /// The client's cache breakpoint; forwarded rather than interpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<Value>,
}

impl Block {
    /// A text block, for the message shapes that stand for one.
    #[must_use]
    pub(crate) fn text(text: impl Into<String>) -> Self {
        Self {
            kind: Some("text".to_owned()),
            text: Some(text.into()),
            ..Self::default()
        }
    }
}

/// An inline or remote image.
///
/// Loosely typed because the two forms share no field names, and a client that
/// sends a third form should keep its request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageSource {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
}

/// `tool_choice` in Anthropic's spelling: `auto`, `any`, `none` or `tool`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolChoice {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The forced tool's name, when `kind` is `tool`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// How much the model should think.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Thinking {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// A `float` rather than an integer so a budget written as `1e4` still
    /// parses; the value is only ever compared against thresholds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<f64>,
    /// The level an `adaptive` budget asks for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// Request attribution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn system_accepts_a_string_or_sections() {
        let text: MessagesRequest = serde_json::from_value(json!({ "system": "hi" })).expect("parse");
        assert_eq!(text.system, Some(SystemPrompt::Text("hi".to_owned())));

        let sections: MessagesRequest = serde_json::from_value(json!({
            "system": [{ "type": "text", "text": "a" }]
        }))
        .expect("parse");
        let Some(SystemPrompt::Sections(sections)) = sections.system else {
            panic!("expected sections");
        };
        assert_eq!(sections.len(), 1);
    }

    #[test]
    fn an_unexpected_system_shape_is_kept_for_the_converter_to_judge() {
        let request: MessagesRequest = serde_json::from_value(json!({ "system": 12 })).expect("parse");
        assert_eq!(request.system, Some(SystemPrompt::Other(json!(12))));
    }

    #[test]
    fn a_request_tolerates_unknown_fields() {
        let request: MessagesRequest = serde_json::from_value(json!({
            "model": "m",
            "messages": [],
            "service_tier": "standard",
            "container": { "id": "c" }
        }))
        .expect("parse");
        assert_eq!(request.model.as_deref(), Some("m"));
    }

    #[test]
    fn a_tool_result_may_carry_text_blocks() {
        let request: MessagesRequest = serde_json::from_value(json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_1",
                    "content": [{ "type": "text", "text": "a" }]
                }]
            }]
        }))
        .expect("parse");
        let Some(MessageContent::Blocks(blocks)) = &request.messages[0].content else {
            panic!("expected blocks");
        };
        assert_eq!(blocks[0].tool_use_id.as_deref(), Some("toolu_1"));
    }
}
