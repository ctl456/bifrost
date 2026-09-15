//! Parsed model output, independent of any protocol.
//!
//! The wire layer decodes its dialect into these chunks; protocol adapters render
//! them back out. Nothing here knows about SSE framing or JSON shapes.

use crate::canonical::{CanonicalResponse, FinishReason, Usage};
use crate::content::ContentBlock;
use crate::error::Error;

/// One piece of parsed upstream output.
#[derive(Debug, Clone, PartialEq)]
pub enum OutputChunk {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolCall {
        id: Option<String>,
        name: String,
        arguments: String,
    },
    /// Token accounting as the upstream reported it.
    ///
    /// Its own chunk rather than a field of [`OutputChunk::Finish`] because the
    /// upstream reports accounting twice — once when a step ends and again in
    /// the terminal event — and both readings are load-bearing: the first is what
    /// the empty-response guard tests, the second is what the client is billed.
    /// Renderers keep the latest value and attach it to the frame they close on.
    Usage(Usage),
    Finish {
        reason: FinishReason,
    },
    /// The upstream reported a failure mid-stream.
    UpstreamError {
        error: Error,
    },
}

/// Stable id for a tool call, synthesizing one when the upstream omitted it.
///
/// Ids are only required to be unique within a response and to survive being
/// echoed back, so a positional id is enough and keeps output reproducible.
#[must_use]
pub fn tool_call_id(id: Option<&str>, index: usize) -> String {
    match id {
        Some(id) if !id.is_empty() => id.to_owned(),
        _ => format!("call_{index}"),
    }
}

/// A tool call assembled from its deltas.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// The argument string exactly as the upstream sent it.
    ///
    /// Kept as text rather than a parsed value: re-serializing would renumber
    /// floats and re-order keys, and clients forward these bytes to tools.
    pub arguments: String,
}

/// Folds a chunk stream into a complete response.
#[derive(Debug, Clone, Default)]
pub struct OutputAccumulator {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<FinishReason>,
    pub usage: Usage,
    pub error: Option<Error>,
}

impl OutputAccumulator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold in one chunk.
    pub fn push(&mut self, chunk: &OutputChunk) {
        match chunk {
            OutputChunk::Text { text } => self.text.push_str(text),
            OutputChunk::Reasoning { text } => self.reasoning.push_str(text),
            OutputChunk::ToolCall { id, name, arguments } => {
                let index = self.tool_calls.len();
                self.tool_calls.push(ToolCall {
                    id: tool_call_id(id.as_deref(), index),
                    name: name.clone(),
                    arguments: arguments.clone(),
                });
            }
            OutputChunk::Usage(usage) => self.usage = *usage,
            OutputChunk::Finish { reason } => self.finish_reason = Some(*reason),
            OutputChunk::UpstreamError { error } => self.error = Some(error.clone()),
        }
    }

    /// Fold in a whole batch.
    pub fn extend(&mut self, chunks: &[OutputChunk]) {
        for chunk in chunks {
            self.push(chunk);
        }
    }

    /// Whether the upstream produced no billable output at all.
    ///
    /// Callers treat this as a failure: a zero-token success would otherwise be
    /// billed downstream as if it were a real completion.
    #[must_use]
    pub fn is_empty_output(&self) -> bool {
        self.error.is_none() && self.usage.completion_tokens == 0
    }

    #[must_use]
    pub fn finish_reason(&self) -> FinishReason {
        self.finish_reason.unwrap_or(FinishReason::Stop)
    }

    /// The conversation the client should see, in the order the upstream
    /// requires it back: reasoning, then text, then tool calls.
    #[must_use]
    pub fn into_response(&self, id: impl Into<String>, model: impl Into<String>, created: i64) -> CanonicalResponse {
        let mut content = Vec::new();
        if !self.reasoning.is_empty() {
            content.push(ContentBlock::reasoning(self.reasoning.clone()));
        }
        if !self.text.is_empty() {
            content.push(ContentBlock::text(self.text.clone()));
        }
        for call in &self.tool_calls {
            content.push(ContentBlock::ToolUse {
                id: call.id.clone(),
                name: call.name.clone(),
                input: parse_arguments(&call.arguments),
            });
        }
        CanonicalResponse {
            id: id.into(),
            model: model.into(),
            created,
            content,
            finish_reason: self.finish_reason(),
            usage: self.usage,
        }
    }
}

/// Best-effort parse of a tool call's argument string.
///
/// Unparseable arguments become `{}` rather than failing the response: a tool
/// call with no arguments is recoverable, a dropped one loses the whole turn.
fn parse_arguments(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::json!({}))
}

/// Renders parsed output into protocol-specific events.
pub trait ChunkGenerator {
    /// A rendered event, ready to be written to the client.
    type Event;

    /// Render one output chunk. Returning nothing means "no client-visible
    /// change", which is normal for deltas the protocol does not surface.
    fn generate(&mut self, chunk: &OutputChunk) -> Vec<Self::Event>;

    /// Events that close the stream after the upstream ended cleanly.
    ///
    /// Separate from what [`ChunkGenerator::generate`] returns for
    /// [`OutputChunk::Finish`] because a protocol's terminator is not a reaction
    /// to upstream output: an upstream that dies without a terminal event must
    /// not look like a clean finish.
    fn end_events(&mut self) -> Vec<Self::Event> {
        Vec::new()
    }

    /// Events that report a failure which happened after the response was
    /// committed.
    ///
    /// Once the status line is on the wire it cannot be revised, so a failure
    /// that arrives later has to be spelled in the protocol's own stream
    /// vocabulary or the client is left reading a truncated stream as a
    /// finished one. The renderer owns the counter the stream has already
    /// advanced, so the frame has to come from it rather than from a fresh one.
    ///
    /// The default is to say nothing, which is what a protocol with no
    /// in-stream failure event has to do: ending the stream is then the only
    /// signal available, and an invented frame would be worse than silence.
    fn failure_frames(&mut self, error: &Error) -> Vec<Self::Event> {
        let _ = error;
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_and_reasoning_accumulate_in_order() {
        let mut accumulator = OutputAccumulator::new();
        accumulator.push(&OutputChunk::Reasoning {
            text: "think ".to_owned(),
        });
        accumulator.push(&OutputChunk::Text {
            text: "hello".to_owned(),
        });
        accumulator.push(&OutputChunk::Text {
            text: " world".to_owned(),
        });

        assert_eq!(accumulator.reasoning, "think ");
        assert_eq!(accumulator.text, "hello world");
        assert_eq!(accumulator.finish_reason(), FinishReason::Stop);
    }

    #[test]
    fn tool_calls_are_kept_in_arrival_order() {
        let mut accumulator = OutputAccumulator::new();
        accumulator.push(&OutputChunk::ToolCall {
            id: Some("call_a".to_owned()),
            name: "one".to_owned(),
            arguments: "{}".to_owned(),
        });
        accumulator.push(&OutputChunk::ToolCall {
            id: None,
            name: "two".to_owned(),
            arguments: "{}".to_owned(),
        });

        assert_eq!(accumulator.tool_calls.len(), 2);
        assert_eq!(accumulator.tool_calls[0].id, "call_a");
        assert_eq!(
            accumulator.tool_calls[1].id, "call_1",
            "a missing id falls back to its position"
        );
    }

    #[test]
    fn an_empty_tool_call_id_is_replaced() {
        assert_eq!(tool_call_id(Some(""), 4), "call_4");
        assert_eq!(tool_call_id(None, 0), "call_0");
        assert_eq!(tool_call_id(Some("keep"), 9), "keep");
    }

    #[test]
    fn the_latest_usage_wins() {
        let mut accumulator = OutputAccumulator::new();
        accumulator.push(&OutputChunk::Usage(Usage {
            prompt_tokens: 10,
            completion_tokens: 3,
            ..Usage::default()
        }));
        accumulator.push(&OutputChunk::Finish {
            reason: FinishReason::ToolCalls,
        });

        assert_eq!(accumulator.finish_reason(), FinishReason::ToolCalls);
        assert_eq!(accumulator.usage.prompt_tokens, 10);
        assert!(!accumulator.is_empty_output());
    }

    #[test]
    fn zero_completion_tokens_counts_as_empty_output() {
        let mut accumulator = OutputAccumulator::new();
        accumulator.push(&OutputChunk::Finish {
            reason: FinishReason::Stop,
        });
        assert!(
            accumulator.is_empty_output(),
            "a zero-token completion must be reported as a failure"
        );

        accumulator.push(&OutputChunk::Text {
            text: "real".to_owned(),
        });
        assert!(
            accumulator.is_empty_output(),
            "text without usage is still unbilled output"
        );
    }

    #[test]
    fn an_upstream_error_is_not_treated_as_empty_output() {
        let mut accumulator = OutputAccumulator::new();
        accumulator.push(&OutputChunk::UpstreamError {
            error: Error::upstream("boom"),
        });
        assert!(!accumulator.is_empty_output());
        assert!(accumulator.error.is_some());
    }

    #[test]
    fn a_response_orders_reasoning_text_then_tool_calls() {
        let mut accumulator = OutputAccumulator::new();
        accumulator.push(&OutputChunk::Text {
            text: "answer".to_owned(),
        });
        accumulator.push(&OutputChunk::Reasoning { text: "why".to_owned() });
        accumulator.push(&OutputChunk::ToolCall {
            id: Some("call_1".to_owned()),
            name: "search".to_owned(),
            arguments: r#"{"q":"x"}"#.to_owned(),
        });
        accumulator.push(&OutputChunk::Usage(Usage {
            completion_tokens: 5,
            ..Usage::default()
        }));
        accumulator.push(&OutputChunk::Finish {
            reason: FinishReason::ToolCalls,
        });

        let response = accumulator.into_response("chatcmpl-1", "m", 7);
        assert_eq!(response.id, "chatcmpl-1");
        assert_eq!(response.finish_reason, FinishReason::ToolCalls);
        assert_eq!(response.usage.completion_tokens, 5);
        assert_eq!(
            serde_json::to_value(&response.content).expect("serialize"),
            json!([
                { "type": "reasoning", "text": "why" },
                { "type": "text", "text": "answer" },
                { "type": "tool_use", "id": "call_1", "name": "search", "input": { "q": "x" } },
            ])
        );
    }

    #[test]
    fn unparseable_tool_arguments_degrade_to_an_empty_object() {
        let mut accumulator = OutputAccumulator::new();
        accumulator.push(&OutputChunk::ToolCall {
            id: None,
            name: "search".to_owned(),
            arguments: "not json".to_owned(),
        });

        let response = accumulator.into_response("id", "m", 0);
        assert_eq!(
            response.content,
            vec![ContentBlock::ToolUse {
                id: "call_0".to_owned(),
                name: "search".to_owned(),
                input: json!({}),
            }]
        );
    }
}
