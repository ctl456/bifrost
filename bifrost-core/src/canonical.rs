//! The intermediate representation every adapter converts to and from.

use serde::{Deserialize, Serialize};

use crate::content::ContentBlock;
use crate::tools::{ToolChoice, ToolDefinition};

/// Output-token ceiling applied when the client sends none.
///
/// Matches the value the official CLI reports, so request bodies stay shape-
/// compatible with real client traffic.
pub const DEFAULT_MAX_TOKENS: u32 = 64_000;

/// Who authored a message.
///
/// `Tool` is its own role because OpenAI treats tool output as a separate
/// message while Anthropic folds it into a `user` turn; normalizing here keeps
/// that difference out of every adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    Tool,
}

/// One turn of the conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalMessage {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl CanonicalMessage {
    #[must_use]
    pub fn new(role: Role, content: Vec<ContentBlock>) -> Self {
        Self { role, content }
    }

    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self::new(Role::User, vec![ContentBlock::text(text)])
    }
}

/// How hard the model should think before answering.
///
/// `Other` exists so an unrecognized level is forwarded verbatim instead of
/// being dropped or coerced. The upstream accepts levels this build has never
/// heard of, and quietly turning the client's `xhigh` into `low` would change
/// the answer while looking like a successful request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    Max,
    Other(String),
}

impl ReasoningEffort {
    /// Classify a level as the client spelled it.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        match raw {
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "max" => Self::Max,
            other => Self::Other(other.to_owned()),
        }
    }

    /// Map Anthropic's `thinking.budget_tokens` onto an effort level.
    ///
    /// Thresholds mirror the JavaScript proxy so existing clients keep the same
    /// behavior when they switch to Bifrost.
    #[must_use]
    pub const fn from_thinking_budget(tokens: u64) -> Self {
        if tokens >= 10_000 {
            Self::High
        } else if tokens >= 5_000 {
            Self::Medium
        } else {
            Self::Low
        }
    }

    /// The 1-100 score DeepSeek V4.1 prompt templates expect.
    ///
    /// Extends the `deepseek-recipe` table (50 / 75 / 100) with an explicit
    /// `Medium` bucket so every variant of this enum maps somewhere.
    #[must_use]
    pub fn score(&self) -> u8 {
        match self {
            Self::Low => 50,
            Self::Medium | Self::High => 75,
            Self::Max => 100,
            Self::Other(_) => 75,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
            Self::Other(raw) => raw,
        }
    }
}

impl Serialize for ReasoningEffort {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ReasoningEffort {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::parse(&String::deserialize(deserializer)?))
    }
}

/// Sampling and budget settings.
///
/// Unspecified values stay `None`; protocol- and model-specific defaults belong
/// to the adapter, not here.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InferenceParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budget_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
}

impl InferenceParams {
    /// The token ceiling to actually send, applying [`DEFAULT_MAX_TOKENS`].
    #[must_use]
    pub fn effective_max_tokens(&self) -> u32 {
        self.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)
    }
}

/// A normalized inbound request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// System prompt blocks, kept separate from `messages` because Anthropic
    /// carries them in a top-level field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<ContentBlock>,
    pub messages: Vec<CanonicalMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    /// `None` when the client sent no choice at all.
    ///
    /// Distinct from `Some(Auto)`: the upstream accepts an explicit
    /// `tool_choice: {type:"auto"}`, and whether the key is present is
    /// observable, so the IR must not collapse the two.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub params: InferenceParams,
    #[serde(default)]
    pub stream: bool,
    /// OpenAI's `prompt_cache_key`.
    ///
    /// Carried through uninterpreted: the IR has no opinion about prompt
    /// caching, and only the cache mechanism decides whether a key becomes a
    /// breakpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    /// The end user the client attributes the request to.
    ///
    /// Kept for the audit trail rather than for the wire: no dialect this build
    /// speaks forwards it, but correlating a request with a person is exactly
    /// what an audit log is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Fields the client's own protocol echoes back in its response.
    ///
    /// The IR normalizes, and normalizing is lossy: OpenAI's Responses API
    /// answers with the `instructions`, `reasoning` and `tools` it was sent, in
    /// the shape it was sent, and an adapter cannot rebuild those from blocks and
    /// definitions without guessing at spellings the client chose. So the adapter
    /// that needs them stores them here, and only that adapter reads them back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub echo: Option<serde_json::Value>,
}

impl CanonicalRequest {
    #[must_use]
    pub fn new(messages: Vec<CanonicalMessage>) -> Self {
        Self {
            model: None,
            system: Vec::new(),
            messages,
            tools: Vec::new(),
            tool_choice: None,
            parallel_tool_calls: None,
            params: InferenceParams::default(),
            stream: false,
            prompt_cache_key: None,
            user: None,
            echo: None,
        }
    }
}

/// Why generation stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Error,
}

impl FinishReason {
    /// The spelling OpenAI-compatible clients understand.
    ///
    /// Deliberately the same string the enum serializes to, so the IR and the
    /// wire form cannot drift apart.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolCalls => "tool_calls",
            Self::ContentFilter => "content_filter",
            Self::Error => "error",
        }
    }
}

/// Token accounting, including cache traffic.
///
/// The field spelling follows OpenAI because that is what the upstream reports,
/// but the record is protocol-neutral: Anthropic calls [`Self::cached_tokens`]
/// `cache_read_input_tokens` and [`Self::cache_write_tokens`]
/// `cache_creation_input_tokens`, so one record serves every adapter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub reasoning_tokens: u32,
    /// Prompt tokens served from the provider cache.
    #[serde(default)]
    pub cached_tokens: u32,
    /// Prompt tokens this request wrote into the provider cache.
    #[serde(default)]
    pub cache_write_tokens: u32,
    /// The upstream's own count of *uncached* prompt tokens.
    ///
    /// Kept as a separate field rather than re-derived: when the upstream
    /// reports it directly it is authoritative, and subtraction drifts whenever
    /// a cache counter is missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_cache_tokens: Option<u32>,
}

impl Usage {
    #[must_use]
    pub const fn total_tokens(&self) -> u32 {
        self.prompt_tokens.saturating_add(self.completion_tokens)
    }

    /// Prompt tokens that were neither read from nor written to the cache.
    ///
    /// Anthropic's `input_tokens` counts only this part, while the upstream's
    /// `inputTokens` is the total including cache traffic. Forwarding the total
    /// would make a client that sums `input_tokens` with the two cache counters
    /// bill roughly twice the real prompt.
    #[must_use]
    pub const fn non_cached_input_tokens(&self) -> u32 {
        if let Some(explicit) = self.no_cache_tokens {
            return explicit;
        }
        self.prompt_tokens
            .saturating_sub(self.cached_tokens)
            .saturating_sub(self.cache_write_tokens)
    }
}

/// A normalized non-streaming response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalResponse {
    pub id: String,
    pub model: String,
    pub created: i64,
    pub content: Vec<ContentBlock>,
    pub finish_reason: FinishReason,
    pub usage: Usage,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn effort_thresholds_match_the_proxy() {
        assert_eq!(ReasoningEffort::from_thinking_budget(1_999), ReasoningEffort::Low);
        assert_eq!(ReasoningEffort::from_thinking_budget(2_000), ReasoningEffort::Low);
        assert_eq!(ReasoningEffort::from_thinking_budget(5_000), ReasoningEffort::Medium);
        assert_eq!(ReasoningEffort::from_thinking_budget(9_999), ReasoningEffort::Medium);
        assert_eq!(ReasoningEffort::from_thinking_budget(10_000), ReasoningEffort::High);
    }

    #[test]
    fn effort_scores_cover_every_variant() {
        assert_eq!(ReasoningEffort::Low.score(), 50);
        assert_eq!(ReasoningEffort::Medium.score(), 75);
        assert_eq!(ReasoningEffort::High.score(), 75);
        assert_eq!(ReasoningEffort::Max.score(), 100);
    }

    #[test]
    fn effective_max_tokens_falls_back_to_the_cli_default() {
        assert_eq!(InferenceParams::default().effective_max_tokens(), DEFAULT_MAX_TOKENS);
        let params = InferenceParams {
            max_tokens: Some(128),
            ..InferenceParams::default()
        };
        assert_eq!(params.effective_max_tokens(), 128);
    }

    #[test]
    fn finish_reasons_use_the_client_spelling() {
        assert_eq!(FinishReason::ToolCalls.as_str(), "tool_calls");
        assert_eq!(
            serde_json::to_value(FinishReason::ToolCalls).expect("serialize"),
            json!("tool_calls")
        );
    }

    #[test]
    fn uncached_input_prefers_the_upstream_count() {
        let usage = Usage {
            prompt_tokens: 7_653,
            cached_tokens: 7_568,
            no_cache_tokens: Some(85),
            ..Usage::default()
        };
        assert_eq!(
            usage.non_cached_input_tokens(),
            85,
            "an explicit count wins over subtraction"
        );

        let derived = Usage {
            no_cache_tokens: None,
            ..usage
        };
        assert_eq!(derived.non_cached_input_tokens(), 85, "subtraction is the fallback");
    }

    #[test]
    fn uncached_input_never_underflows() {
        let usage = Usage {
            prompt_tokens: 10,
            cached_tokens: 8,
            cache_write_tokens: 8,
            ..Usage::default()
        };
        assert_eq!(usage.non_cached_input_tokens(), 0);
    }

    #[test]
    fn usage_total_saturates_instead_of_overflowing() {
        let usage = Usage {
            prompt_tokens: u32::MAX,
            completion_tokens: 10,
            ..Usage::default()
        };
        assert_eq!(usage.total_tokens(), u32::MAX);
    }

    #[test]
    fn empty_optional_fields_are_omitted() {
        let request = CanonicalRequest::new(vec![CanonicalMessage::user("hi")]);
        assert_eq!(
            serde_json::to_value(&request).expect("serialize"),
            json!({
                "messages": [{ "role": "user", "content": [{ "type": "text", "text": "hi" }] }],
                "params": {},
                "stream": false
            })
        );
    }
}
