//! Canonical output → Chat Completions frames.
//!
//! Two behaviors here are deliberate and were learned the hard way upstream:
//!
//! - **A `Usage` chunk emits nothing.** Accounting is attached to the frame that
//!   closes the stream, not to a frame of its own, because clients that read only
//!   deltas would otherwise see an extra choice-less chunk.
//! - **An upstream error emits no finish frame.** Downstream agent loops stop at
//!   the first `finish_reason`, so emitting one before the error would hide it.

use bifrost_core::{ChunkGenerator, FinishReason, OutputChunk, Usage};
use serde_json::{Value, json};

use crate::openai::shared::OpenAiUsage;
use crate::sse::{DONE, SseFrame};

/// Renders canonical chunks as Chat Completions SSE frames.
#[derive(Debug, Clone)]
pub struct ChatStreamRenderer {
    model: String,
    id: String,
    created: i64,
    chunks_emitted: usize,
    tool_call_index: usize,
    usage: Usage,
    finish_reason: Option<FinishReason>,
    started: bool,
}

impl ChatStreamRenderer {
    #[must_use]
    pub fn new(model: impl Into<String>, id: impl Into<String>, created: i64) -> Self {
        Self {
            model: model.into(),
            id: id.into(),
            created,
            chunks_emitted: 0,
            tool_call_index: 0,
            usage: Usage::default(),
            finish_reason: None,
            started: false,
        }
    }

    /// Whether anything has been written to the client yet.
    ///
    /// The HTTP layer uses this to keep the response uncommitted until the first
    /// real output, so a request that times out or returns nothing can still be
    /// answered with a JSON error and a status the client will retry on.
    #[must_use]
    pub const fn started(&self) -> bool {
        self.started
    }

    /// The reason the upstream gave for stopping, once it has.
    ///
    /// The edge reads this to tell "finished" from "the transport died", which
    /// decides whether a truncated stream is reported as an error.
    #[must_use]
    pub const fn finish_reason(&self) -> Option<FinishReason> {
        self.finish_reason
    }

    /// Turn one canonical chunk into frames.
    pub fn render(&mut self, chunk: &OutputChunk) -> Vec<SseFrame> {
        match chunk {
            OutputChunk::Text { text } => self.delta(json!({ "content": text }), false),
            OutputChunk::Reasoning { text } => self.delta(json!({ "reasoning_content": text }), false),
            OutputChunk::ToolCall { id, name, arguments } => {
                let id = bifrost_core::tool_call_id(id.as_deref(), self.tool_call_index);
                let index = self.tool_call_index;
                self.tool_call_index += 1;
                self.delta(
                    json!({
                        "tool_calls": [{
                            "index": index,
                            "id": id,
                            "type": "function",
                            "function": { "name": name, "arguments": arguments },
                        }],
                    }),
                    true,
                )
            }
            // Attached to the closing frame instead; see the module comment.
            OutputChunk::Usage(usage) => {
                self.usage = *usage;
                Vec::new()
            }
            // Reason and accounting travel in one frame: a client that reads
            // usage only from the terminal frame would otherwise see two
            // terminal frames, the second with no reason to attach it to.
            OutputChunk::Finish { reason } => {
                self.finish_reason = Some(*reason);
                self.started = true;
                vec![self.frame(json!({}), Some(reason.as_str()), Some(OpenAiUsage::from_ir(self.usage)))]
            }
            OutputChunk::UpstreamError { .. } => {
                // The error frame is emitted by the caller, which owns the
                // protocol-level error body. Emitting a finish frame here would
                // hide the error from agent loops that stop at the first one.
                Vec::new()
            }
        }
    }

    /// The `[DONE]` sentinel that closes a stream which ended cleanly.
    #[must_use]
    pub fn done_frame() -> SseFrame {
        SseFrame::from_raw(DONE)
    }

    /// Frames for a client that disconnected mid-stream.
    ///
    /// The client is gone, so these are for whoever observes the connection: a
    /// terminating frame with zero usage keeps a downstream tracker from
    /// estimating the tokens of a request it never saw finish.
    #[must_use]
    pub fn aborted_frames(model: &str, id: &str, created: i64) -> Vec<SseFrame> {
        let renderer = Self::new(model, id, created);
        vec![
            renderer.frame(
                json!({}),
                Some(FinishReason::Stop.as_str()),
                Some(OpenAiUsage::default()),
            ),
            Self::done_frame(),
        ]
    }

    /// Build a chunk, attaching the role on the very first delta.
    fn delta(&mut self, mut delta: Value, clear_content: bool) -> Vec<SseFrame> {
        if self.chunks_emitted == 0 {
            if let Value::Object(map) = &mut delta {
                let mut with_role = serde_json::Map::new();
                with_role.insert("role".to_owned(), json!("assistant"));
                if clear_content {
                    with_role.insert("content".to_owned(), Value::Null);
                }
                with_role.extend(std::mem::take(map));
                *map = with_role;
            }
        }
        self.chunks_emitted += 1;
        self.started = true;
        vec![self.frame(delta, None, None)]
    }

    fn frame(&self, delta: Value, finish_reason: Option<&str>, usage: Option<OpenAiUsage>) -> SseFrame {
        let mut choice = serde_json::Map::new();
        choice.insert("index".to_owned(), json!(0));
        choice.insert("delta".to_owned(), delta);
        choice.insert(
            "finish_reason".to_owned(),
            finish_reason.map_or(Value::Null, |reason| json!(reason)),
        );

        let mut chunk = serde_json::Map::new();
        chunk.insert("id".to_owned(), json!(self.id));
        chunk.insert("object".to_owned(), json!("chat.completion.chunk"));
        chunk.insert("created".to_owned(), json!(self.created));
        chunk.insert("model".to_owned(), json!(self.model));
        chunk.insert("choices".to_owned(), Value::Array(vec![Value::Object(choice)]));
        if let Some(usage) = usage {
            chunk.insert("usage".to_owned(), json!(usage));
        }
        SseFrame::data(&Value::Object(chunk))
    }
}

/// Render a complete (non-streaming) response body.
///
/// Built from the canonical response rather than from accumulated deltas: the
/// streaming and non-streaming paths then agree by construction, and a difference
/// between them is a bug in one renderer instead of a divergence between two.
#[must_use]
pub fn completion_body(meta: &crate::adapter::ResponseMeta, response: &bifrost_core::CanonicalResponse) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for block in &response.content {
        match block {
            bifrost_core::ContentBlock::Text { text: part, .. } => text.push_str(part),
            bifrost_core::ContentBlock::Reasoning { text: part } => reasoning.push_str(part),
            bifrost_core::ContentBlock::ToolUse { id, name, input } => tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    // Re-encoded rather than remembered: the non-streaming path
                    // has no raw argument string to forward, and the upstream
                    // already sent these bytes as parsed JSON.
                    "arguments": input.to_string(),
                },
            })),
            _ => {}
        }
    }

    let mut message = serde_json::Map::new();
    message.insert("role".to_owned(), json!("assistant"));
    // `null` rather than omitted: clients distinguish "no text" from "field
    // absent" when a turn is tool calls only.
    message.insert(
        "content".to_owned(),
        if text.is_empty() { Value::Null } else { json!(text) },
    );
    if !tool_calls.is_empty() {
        message.insert("tool_calls".to_owned(), Value::Array(tool_calls));
    }
    if !reasoning.is_empty() {
        message.insert("reasoning_content".to_owned(), json!(reasoning));
    }

    let mut choice = serde_json::Map::new();
    choice.insert("index".to_owned(), json!(0));
    choice.insert("message".to_owned(), Value::Object(message));
    choice.insert("finish_reason".to_owned(), json!(response.finish_reason.as_str()));

    let mut body = serde_json::Map::new();
    body.insert("id".to_owned(), json!(meta.id));
    body.insert("object".to_owned(), json!("chat.completion"));
    body.insert("created".to_owned(), json!(meta.created));
    body.insert("model".to_owned(), json!(meta.model));
    body.insert("choices".to_owned(), Value::Array(vec![Value::Object(choice)]));
    body.insert("usage".to_owned(), json!(OpenAiUsage::from_ir(response.usage)));
    Value::Object(body)
}

impl ChunkGenerator for ChatStreamRenderer {
    type Event = SseFrame;

    fn generate(&mut self, chunk: &OutputChunk) -> Vec<SseFrame> {
        self.render(chunk)
    }

    fn end_events(&mut self) -> Vec<SseFrame> {
        vec![Self::done_frame()]
    }

    fn failure_frames(&mut self, error: &bifrost_core::Error) -> Vec<SseFrame> {
        vec![failure_frame(error)]
    }
}

/// The frame a mid-stream failure is reported with.
///
/// Unnamed, like every other frame this endpoint sends, and it carries no
/// `choices`: there is no completion to attach a finish reason to, and inventing
/// one would tell the client a broken turn was finished.
#[must_use]
pub fn failure_frame(error: &bifrost_core::Error) -> SseFrame {
    let mut envelope = serde_json::Map::new();
    envelope.insert("message".to_owned(), json!(error.message));
    envelope.insert("type".to_owned(), json!(error.kind.wire_type()));
    if let Some(code) = error.upstream_code.as_ref() {
        envelope.insert("code".to_owned(), json!(code));
    }
    let mut body = serde_json::Map::new();
    body.insert("error".to_owned(), Value::Object(envelope));
    if let Some(retry_after) = error.retry_after {
        body.insert("retry_after".to_owned(), json!(retry_after));
    }
    SseFrame::data(&Value::Object(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bifrost_core::OutputAccumulator;

    fn renderer() -> ChatStreamRenderer {
        ChatStreamRenderer::new("m", "chatcmpl-1", 1_700_000_000)
    }

    fn frames_text(renderer: &mut ChatStreamRenderer, chunk: &OutputChunk) -> Vec<String> {
        renderer
            .render(chunk)
            .iter()
            .map(|frame| frame.as_str().to_owned())
            .collect()
    }

    #[test]
    fn the_first_delta_carries_the_role() {
        let mut renderer = renderer();
        let frames = frames_text(&mut renderer, &OutputChunk::Text { text: "hi".to_owned() });
        assert_eq!(frames.len(), 1);
        assert!(
            frames[0]
                .as_str()
                .contains(r#""delta":{"role":"assistant","content":"hi"}"#),
            "{}",
            frames[0]
        );
        assert!(renderer.started());
    }

    #[test]
    fn later_deltas_carry_no_role() {
        let mut renderer = renderer();
        frames_text(&mut renderer, &OutputChunk::Text { text: "a".to_owned() });
        let frames = frames_text(&mut renderer, &OutputChunk::Text { text: "b".to_owned() });
        assert!(
            frames[0].as_str().contains(r#""delta":{"content":"b"}"#),
            "{}",
            frames[0]
        );
    }

    #[test]
    fn a_tool_call_delta_carries_index_id_and_arguments() {
        let mut renderer = renderer();
        let frames = frames_text(
            &mut renderer,
            &OutputChunk::ToolCall {
                id: Some("call_x".to_owned()),
                name: "search".to_owned(),
                arguments: r#"{"q":1}"#.to_owned(),
            },
        );
        assert!(
            frames[0].as_str().contains(
                r#""delta":{"role":"assistant","content":null,"tool_calls":[{"index":0,"id":"call_x","type":"function","function":{"name":"search","arguments":"{\"q\":1}"}}]}"#
            ),
            "{}",
            frames[0]
        );
    }

    #[test]
    fn a_tool_call_without_an_id_gets_a_positional_one() {
        let mut renderer = renderer();
        let frames = frames_text(
            &mut renderer,
            &OutputChunk::ToolCall {
                id: None,
                name: "search".to_owned(),
                arguments: "{}".to_owned(),
            },
        );
        assert!(frames[0].as_str().contains(r#""id":"call_0""#), "{}", frames[0]);
    }

    #[test]
    fn usage_alone_emits_nothing() {
        let mut renderer = renderer();
        let frames = frames_text(
            &mut renderer,
            &OutputChunk::Usage(Usage {
                prompt_tokens: 5,
                completion_tokens: 2,
                ..Usage::default()
            }),
        );
        assert!(frames.is_empty());
        assert!(!renderer.started(), "accounting must not commit the response");
    }

    #[test]
    fn the_finish_chunk_carries_reason_and_usage_together() {
        let mut renderer = renderer();
        frames_text(&mut renderer, &OutputChunk::Text { text: "hi".to_owned() });
        frames_text(
            &mut renderer,
            &OutputChunk::Usage(Usage {
                prompt_tokens: 5,
                completion_tokens: 2,
                cached_tokens: 1,
                ..Usage::default()
            }),
        );
        let frames = frames_text(
            &mut renderer,
            &OutputChunk::Finish {
                reason: FinishReason::Stop,
            },
        );
        assert_eq!(frames.len(), 1, "reason and usage must land in one frame");
        assert!(frames[0].as_str().contains(r#""delta":{}"#), "{}", frames[0]);
        assert!(
            frames[0].as_str().contains(r#""finish_reason":"stop""#),
            "{}",
            frames[0]
        );
        assert!(
            frames[0].as_str().contains(r#""usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7,"prompt_tokens_details":{"cached_tokens":1}}"#),
            "{}",
            frames[0]
        );
    }

    #[test]
    fn an_ending_without_a_finish_event_says_nothing_about_why() {
        let mut renderer = renderer();
        frames_text(&mut renderer, &OutputChunk::Text { text: "hi".to_owned() });
        let frames = renderer.end_events();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].as_str(),
            "data: [DONE]\n\n",
            "a truncated stream must not claim `stop`"
        );
    }

    #[test]
    fn a_disconnect_closes_with_zero_usage() {
        let frames = ChatStreamRenderer::aborted_frames("m", "chatcmpl-1", 7);
        assert_eq!(frames.len(), 2);
        assert!(
            frames[0].as_str().contains(r#""finish_reason":"stop""#),
            "{}",
            frames[0]
        );
        assert!(frames[0].as_str().contains(r#""completion_tokens":0"#), "{}", frames[0]);
        assert_eq!(frames[1].as_str(), "data: [DONE]\n\n");
    }

    #[test]
    fn an_upstream_error_emits_no_frame() {
        let mut renderer = renderer();
        let frames = frames_text(
            &mut renderer,
            &OutputChunk::UpstreamError {
                error: bifrost_core::Error::upstream("boom"),
            },
        );
        assert!(frames.is_empty(), "an error must not be preceded by a finish frame");
    }

    fn meta() -> crate::adapter::ResponseMeta {
        crate::adapter::ResponseMeta {
            id: "chatcmpl-1".to_owned(),
            model: "m".to_owned(),
            created: 12,
            completed: 12,
        }
    }

    #[test]
    fn a_non_streaming_body_omits_empty_text_but_keeps_tool_calls() {
        let mut accumulator = OutputAccumulator::new();
        accumulator.push(&OutputChunk::ToolCall {
            id: Some("call_1".to_owned()),
            name: "search".to_owned(),
            arguments: r#"{"q":"x"}"#.to_owned(),
        });
        accumulator.push(&OutputChunk::Usage(Usage {
            prompt_tokens: 3,
            completion_tokens: 4,
            ..Usage::default()
        }));
        accumulator.push(&OutputChunk::Finish {
            reason: FinishReason::ToolCalls,
        });
        let response = accumulator.into_response("chatcmpl-1", "m", 12);

        let body = completion_body(&meta(), &response);
        assert_eq!(body["choices"][0]["message"]["content"], Value::Null);
        assert_eq!(body["choices"][0]["message"]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            body["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"q\":\"x\"}"
        );
        assert_eq!(body["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(body["usage"]["total_tokens"], 7);
        assert_eq!(body["object"], "chat.completion");
    }

    #[test]
    fn a_non_streaming_body_keeps_reasoning_and_text() {
        let mut accumulator = OutputAccumulator::new();
        accumulator.push(&OutputChunk::Reasoning { text: "why".to_owned() });
        accumulator.push(&OutputChunk::Text {
            text: "answer".to_owned(),
        });
        let response = accumulator.into_response("chatcmpl-1", "m", 12);

        let body = completion_body(&meta(), &response);
        assert_eq!(body["choices"][0]["message"]["reasoning_content"], "why");
        assert_eq!(body["choices"][0]["message"]["content"], "answer");
        assert_eq!(body["choices"][0]["message"].get("tool_calls"), None);
    }

    #[test]
    fn the_message_keys_keep_the_client_order() {
        let mut accumulator = OutputAccumulator::new();
        accumulator.push(&OutputChunk::Reasoning { text: "why".to_owned() });
        accumulator.push(&OutputChunk::Text {
            text: "answer".to_owned(),
        });
        accumulator.push(&OutputChunk::ToolCall {
            id: Some("call_1".to_owned()),
            name: "search".to_owned(),
            arguments: "{}".to_owned(),
        });
        let response = accumulator.into_response("chatcmpl-1", "m", 12);

        let body = completion_body(&meta(), &response);
        let raw = body.to_string();
        let role = raw.find("\"role\"").expect("role");
        let content = raw.find("\"content\"").expect("content");
        let calls = raw.find("\"tool_calls\"").expect("tool_calls");
        let reasoning = raw.find("\"reasoning_content\"").expect("reasoning");
        assert!(role < content && content < calls && calls < reasoning, "{raw}");
    }
}
