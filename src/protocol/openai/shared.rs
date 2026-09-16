//! Vocabulary both OpenAI dialects spell the same way.

use serde::{Deserialize, Serialize};

/// A tool the model may call, in OpenAI's nested `function` shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionTool {
    #[serde(default = "default_tool_type")]
    pub r#type: String,
    pub function: FunctionDefinition,
}

fn default_tool_type() -> String {
    "function".to_owned()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionDefinition {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
    /// Accepted and ignored.
    ///
    /// Strict function calling is an upstream-side feature; the wire protocol
    /// here has no equivalent, so the field is carried through the schema to
    /// avoid rejecting the request rather than dropped silently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// `tool_choice` as a string (`auto`, `none`, `required`) or a forced function.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OpenAiToolChoice {
    Mode(String),
    Function {
        r#type: String,
        #[serde(default)]
        function: Option<NamedFunction>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedFunction {
    pub name: String,
}

/// Token accounting as these endpoints report it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAiUsage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokenDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<CompletionTokenDetails>,
}

impl OpenAiUsage {
    /// Build from canonical accounting.
    ///
    /// `prompt_tokens_details` is always present, even at zero: the original
    /// proxy emitted it on every frame, and clients that read
    /// `cached_tokens` treat its absence differently from zeros.
    #[must_use]
    pub fn from_ir(usage: crate::core::Usage) -> Self {
        Self {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens(),
            prompt_tokens_details: Some(PromptTokenDetails {
                cached_tokens: usage.cached_tokens,
            }),
            completion_tokens_details: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptTokenDetails {
    #[serde(default)]
    pub cached_tokens: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionTokenDetails {
    #[serde(default)]
    pub reasoning_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_details_are_always_present() {
        let usage = OpenAiUsage::from_ir(crate::core::Usage {
            prompt_tokens: 10,
            completion_tokens: 2,
            cached_tokens: 4,
            ..crate::core::Usage::default()
        });
        assert_eq!(usage.total_tokens, 12);
        assert_eq!(
            serde_json::to_value(usage).expect("serialize"),
            serde_json::json!({
                "prompt_tokens": 10,
                "completion_tokens": 2,
                "total_tokens": 12,
                "prompt_tokens_details": { "cached_tokens": 4 },
            })
        );
    }
}
