//! The OpenAI Responses wire types.
//!
//! These describe what a *client* sends. The half of the request the endpoint
//! only echoes back — `instructions`, `reasoning`, `tools` — is kept as raw JSON
//! rather than typed: it has to reach the response in the shape the client chose,
//! and a type here would rebuild it into a shape of this module's choosing.
//!
//! `input` is the exception, because it is the part that becomes the
//! conversation. It is a union of a plain string and a list of items, and the
//! item variants are duck-typed exactly as the original reads them: an item's
//! meaning comes from its `type`, or from its `role` when it never had one.

use serde::{Deserialize, Serialize};

/// A `/v1/responses` request body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponsesRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The conversation, as one string or as a list of items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<ResponsesInput>,
    /// The system prompt, in whatever spelling the client used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Function definitions, in this endpoint's flat shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<serde_json::Value>>,
    /// A mode name, or an object naming one function.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// The reasoning configuration; only `effort` is read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<serde_json::Value>,
    /// Refused rather than accepted: this proxy keeps no conversation state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    /// Accepted and ignored, as the original does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    /// Accepted and ignored; the endpoint reports `user: null` regardless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Accepted and ignored; the endpoint reports `metadata: {}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// `input` is either one string or a list of items.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesInput {
    Text(String),
    Items(Vec<ResponsesItem>),
}

/// One entry of the `input` list.
///
/// One permissive struct rather than a tagged enum: an unrecognized item is
/// dropped on its own instead of failing the whole request, and every field is
/// optional because which ones are present is what identifies the item.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResponsesItem {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// A string, or a list of parts carrying `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,
    /// A reasoning item's summary, as a list of parts carrying `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<serde_json::Value>,
    /// A reasoning item's whole answer, when it has no summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The call a `function_call_output` answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// The call id, when the client used the item's own id instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A `function_call`'s arguments: an object, or an already-encoded string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Value>,
    /// A `function_call_output`'s result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
}

impl ResponsesItem {
    /// What this item is: its `type`, or `message` when it only named a role.
    ///
    /// The fallback is not cosmetic. `type` is optional on the message variant of
    /// the input union, and clients write `{"role":"user","content":"hi"}` — so
    /// without it such a turn is not recognized as anything and is dropped, which
    /// the client never learns about because the request still succeeds.
    #[must_use]
    pub fn kind(&self) -> Option<&str> {
        match self.kind.as_deref() {
            Some(kind) => Some(kind),
            None if self.role.is_some() => Some("message"),
            None => None,
        }
    }
}
