//! Canonical output → Anthropic Messages frames.
//!
//! Two behaviors here are deliberate and were learned the hard way upstream:
//!
//! - **A response with no output is reported as a failure.** A zero-token
//!   success would be billed downstream as a real completion, so the stream ends
//!   on an `error` event rather than on a `message_stop`.
//! - **A thinking block is signed as it closes.** Clients refuse to render a
//!   thinking block that carries no signature, so one is minted for it at the
//!   moment the block ends — see [`crate::protocol::anthropic::signature`].
//!
//! A failure is *not* framed here. The edge owns the choice between an SSE
//! `error` event and a JSON body with a retryable status, and it can only make
//! that choice while it still knows whether anything has been written.

use crate::core::{CanonicalResponse, ChunkGenerator, ContentBlock, Error, FinishReason, OutputChunk, Usage};
use serde_json::{Map, Value, json};

use crate::protocol::adapter::ResponseMeta;
use crate::protocol::anthropic::signature::fake_thinking_signature;
use crate::protocol::sse::SseFrame;

/// What this endpoint says when the upstream produced nothing.
const ZERO_OUTPUT_MESSAGE: &str = "Empty response from upstream (zero output tokens)";

/// The retry hint that goes with [`ZERO_OUTPUT_MESSAGE`].
const ZERO_OUTPUT_RETRY_AFTER: u32 = 10;

/// Render a complete (non-streaming) response body.
///
/// Built from the canonical response rather than from accumulated deltas, so the
/// streaming and non-streaming paths agree by construction.
#[must_use]
pub fn message_body(meta: &ResponseMeta, response: &CanonicalResponse) -> Value {
    let mut content = Vec::new();
    let mut text_length = 0;
    let mut thinking_length = 0;
    let mut tool_calls = 0;
    for block in &response.content {
        match block {
            ContentBlock::Reasoning { text } if !text.is_empty() => {
                thinking_length += utf16_length(text);
                content.push(json!({
                    "type": "thinking",
                    "thinking": text,
                    "signature": fake_thinking_signature(text),
                }));
            }
            ContentBlock::Text { text, .. } if !text.is_empty() => {
                text_length += utf16_length(text);
                content.push(json!({ "type": "text", "text": text }));
            }
            ContentBlock::ToolUse { id, name, input } => {
                tool_calls += 1;
                content.push(json!({ "type": "tool_use", "id": id, "name": name, "input": input }));
            }
            _ => {}
        }
    }

    let mut body = Map::new();
    body.insert("id".to_owned(), json!(meta.id));
    body.insert("type".to_owned(), json!("message"));
    body.insert("role".to_owned(), json!("assistant"));
    body.insert("model".to_owned(), json!(meta.model));
    body.insert("content".to_owned(), Value::Array(content));
    body.insert("stop_reason".to_owned(), json!(stop_reason(response.finish_reason)));
    // Always present and always null: this endpoint reports the sequence that
    // stopped generation, and a gateway that stopped it has none.
    body.insert("stop_sequence".to_owned(), Value::Null);
    body.insert(
        "usage".to_owned(),
        usage_body(response.usage, text_length, thinking_length, tool_calls),
    );
    Value::Object(body)
}

/// Accounting as this endpoint reports it.
///
/// `input_tokens` counts only the uncached part of the prompt. The client-facing
/// documentation defines the total as the sum of `input_tokens` and the two cache
/// counters, so forwarding the upstream's total here would make a client that
/// adds them up count the cached prefix twice.
fn usage_body(usage: Usage, text_length: usize, thinking_length: usize, tool_calls: usize) -> Value {
    // A response with no output reports no input either: a request that produced
    // nothing must not be billed as one that read the prompt.
    let billed_input = usage.completion_tokens != 0;
    json!({
        "input_tokens": if billed_input { usage.non_cached_input_tokens() } else { 0 },
        "output_tokens": if billed_input {
            usage.completion_tokens
        } else {
            estimated_output_tokens(text_length, thinking_length, tool_calls)
        },
        "cache_creation_input_tokens": usage.cache_write_tokens,
        "cache_read_input_tokens": if billed_input { usage.cached_tokens } else { 0 },
    })
}

/// The output-token count to report when the upstream reported none.
///
/// A client shown zero output for an answer it can see treats the response as
/// broken, so the count is estimated from the content. The divisor is the usual
/// bytes-per-token rule of thumb, and UTF-16 units are what is measured because
/// that is what the original measured.
fn estimated_output_tokens(text_length: usize, thinking_length: usize, tool_calls: usize) -> u32 {
    let estimated = (text_length + thinking_length).div_ceil(4) + tool_calls * 20;
    u32::try_from(estimated.max(1)).unwrap_or(u32::MAX)
}

/// How many UTF-16 units a string is, which is what a JavaScript caller counts.
fn utf16_length(text: &str) -> usize {
    text.encode_utf16().count()
}

/// This endpoint's name for a stop reason.
fn stop_reason(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::ToolCalls => "tool_use",
        FinishReason::Length => "max_tokens",
        // Everything else — a plain stop, a filtered answer, a reason this build
        // does not model — is a turn that ended, which is the only other thing
        // this endpoint names.
        _ => "end_turn",
    }
}

/// The frame an in-flight failure is reported with.
///
/// Paired with [`error_body`] so the edge can send the same failure as a status
/// instead, which is what an SDK knows how to back off from.
#[must_use]
pub fn error_frame(error: &Error) -> SseFrame {
    SseFrame::named("error", &error_body(error))
}

/// The `error` event's payload.
#[must_use]
pub fn error_body(error: &Error) -> Value {
    // The message leads, then the classification, then the upstream's own code.
    // Key order is part of what a client sees, and a log line that a human reads
    // should start with what went wrong.
    let mut detail = Map::new();
    detail.insert("message".to_owned(), json!(error.message));
    detail.insert("type".to_owned(), json!(error.kind.wire_type()));
    if let Some(code) = &error.upstream_code {
        detail.insert("code".to_owned(), json!(code));
    }
    json!({ "type": "error", "error": Value::Object(detail) })
}

/// The keepalive for a stream that has gone quiet.
///
/// This endpoint has a named event for it, unlike the OpenAI-shaped ones, which
/// use a comment line. Clients ignore both.
#[must_use]
pub fn ping_frame() -> SseFrame {
    SseFrame::named("ping", &json!({ "type": "ping" }))
}

/// Which block is open, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
}

/// Renders canonical chunks as Anthropic event frames.
#[derive(Debug, Clone)]
pub struct AnthropicStreamRenderer {
    model: String,
    id: String,
    /// Index the next block will be given.
    next_block_index: usize,
    /// The block currently open, with the index it was given.
    open_block: Option<(usize, BlockKind)>,
    /// Whether `message_start` has been sent, which is the same question as
    /// whether any bytes have.
    message_started: bool,
    /// The thinking of the open block, signed when it closes.
    thinking: String,
    output_tokens: u32,
    usage: Usage,
    stop_reason: Option<&'static str>,
    has_error: bool,
}

impl AnthropicStreamRenderer {
    #[must_use]
    pub fn new(model: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            id: id.into(),
            next_block_index: 0,
            open_block: None,
            message_started: false,
            thinking: String::new(),
            output_tokens: 0,
            usage: Usage::default(),
            stop_reason: None,
            has_error: false,
        }
    }

    /// Whether anything has been written to the client yet.
    ///
    /// The HTTP layer uses this to keep the response uncommitted, so a request
    /// that fails or returns nothing can still be answered with a status the
    /// client will retry on.
    #[must_use]
    pub const fn started(&self) -> bool {
        self.message_started
    }

    /// Whether the upstream failed mid-stream.
    ///
    /// A stream that failed is never closed with a terminal event: downstream
    /// loops stop at the first `message_stop`, so one would hide the failure.
    #[must_use]
    pub const fn has_error(&self) -> bool {
        self.has_error
    }

    /// Turn one canonical chunk into frames.
    pub fn render(&mut self, chunk: &OutputChunk) -> Vec<SseFrame> {
        match chunk {
            OutputChunk::Text { text } => {
                let mut frames = self.lead();
                frames.extend(self.open_block(BlockKind::Text));
                frames.push(self.delta(json!({ "type": "text_delta", "text": text })));
                // Counted per delta rather than measured: the upstream reports a
                // total only at the end, and the guard that decides whether the
                // answer is empty runs before that.
                self.output_tokens += 1;
                frames
            }
            OutputChunk::Reasoning { text } => {
                let mut frames = self.lead();
                frames.extend(self.open_block(BlockKind::Thinking));
                self.thinking.push_str(text);
                frames.push(self.delta(json!({ "type": "thinking_delta", "thinking": text })));
                frames
            }
            OutputChunk::ToolCall { id, name, arguments } => {
                let mut frames = self.lead();
                frames.extend(self.close_block());
                let index = self.next_block_index;
                self.next_block_index += 1;
                // A call the upstream left unnamed still needs an id for the
                // client to answer with, so one is synthesized from its position.
                let call_id = id
                    .clone()
                    .filter(|id| !id.is_empty())
                    .unwrap_or_else(|| format!("toolu_{index}"));
                frames.push(SseFrame::named(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": { "type": "tool_use", "id": call_id, "name": name, "input": {} },
                    }),
                ));
                frames.push(SseFrame::named(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "input_json_delta", "partial_json": arguments },
                    }),
                ));
                frames.push(SseFrame::named(
                    "content_block_stop",
                    &json!({ "type": "content_block_stop", "index": index }),
                ));
                // A call weighs more than one token, and the guard above compares
                // against zero, so the exact weight only matters when a turn is
                // nothing but calls.
                self.output_tokens += 20;
                frames
            }
            // Attached to the closing frame instead; see the module comment.
            OutputChunk::Usage(usage) => {
                // The upstream's own total beats the local count of deltas, which
                // is only ever an estimate. A zero is read as "not reported",
                // because the wire zeroes every counter for a step that produced
                // nothing and a stream that already sent deltas cannot be an
                // empty response.
                if usage.completion_tokens > 0 {
                    self.output_tokens = usage.completion_tokens;
                }
                self.usage = *usage;
                Vec::new()
            }
            // The reason is held for the closing frame, which is the only place
            // this endpoint reports it.
            OutputChunk::Finish { reason } => {
                self.stop_reason = Some(stop_reason(*reason));
                Vec::new()
            }
            // Reported by the edge, which knows whether a status is still
            // possible.
            OutputChunk::UpstreamError { .. } => {
                self.has_error = true;
                Vec::new()
            }
        }
    }

    /// Frames that close a stream the upstream ended.
    pub fn end_events(&mut self) -> Vec<SseFrame> {
        if self.has_error || !self.message_started {
            return Vec::new();
        }
        let mut frames = self.close_block();
        if self.output_tokens == 0 {
            frames.push(Self::zero_output_frame());
        } else {
            frames.push(self.message_delta_frame());
            frames.push(Self::message_stop_frame());
        }
        frames
    }

    /// Frames for a client that disconnected mid-stream.
    ///
    /// The client is gone, so these are for whoever observes the connection: a
    /// terminating pair with zero usage keeps a downstream tracker from
    /// estimating the tokens of a request it never saw finish.
    #[must_use]
    pub fn aborted_frames() -> Vec<SseFrame> {
        vec![
            SseFrame::named(
                "message_delta",
                &json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "end_turn" },
                    "usage": { "output_tokens": 0, "input_tokens": 0, "cache_read_input_tokens": 0 },
                }),
            ),
            Self::message_stop_frame(),
        ]
    }

    /// The first event of every stream, sent once.
    fn lead(&mut self) -> Vec<SseFrame> {
        if self.message_started {
            return Vec::new();
        }
        self.message_started = true;
        vec![SseFrame::named(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": self.model,
                    // The prompt's accounting is only known at the end, so this
                    // opening frame reports zeroes by definition.
                    "usage": { "input_tokens": 0, "output_tokens": 0 },
                },
            }),
        )]
    }

    /// Open a block, closing whatever was open first.
    fn open_block(&mut self, kind: BlockKind) -> Vec<SseFrame> {
        if self.open_block.is_some_and(|(_, open)| open == kind) {
            return Vec::new();
        }
        let mut frames = self.close_block();
        let index = self.next_block_index;
        self.next_block_index += 1;
        self.open_block = Some((index, kind));
        let content_block = match kind {
            BlockKind::Text => json!({ "type": "text", "text": "" }),
            BlockKind::Thinking => json!({ "type": "thinking", "thinking": "" }),
        };
        frames.push(SseFrame::named(
            "content_block_start",
            &json!({ "type": "content_block_start", "index": index, "content_block": content_block }),
        ));
        frames
    }

    /// Close the open block, signing it if it is a thinking block.
    fn close_block(&mut self) -> Vec<SseFrame> {
        let Some((index, kind)) = self.open_block.take() else {
            return Vec::new();
        };
        let mut frames = Vec::new();
        if kind == BlockKind::Thinking {
            let signature = fake_thinking_signature(&std::mem::take(&mut self.thinking));
            frames.push(SseFrame::named(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": { "type": "signature_delta", "signature": signature },
                }),
            ));
        }
        frames.push(SseFrame::named(
            "content_block_stop",
            &json!({ "type": "content_block_stop", "index": index }),
        ));
        frames
    }

    /// A delta for the open block.
    fn delta(&self, delta: Value) -> SseFrame {
        let index = self.open_block.map_or(0, |(index, _)| index);
        SseFrame::named(
            "content_block_delta",
            &json!({ "type": "content_block_delta", "index": index, "delta": delta }),
        )
    }

    fn message_delta_frame(&self) -> SseFrame {
        SseFrame::named(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": { "stop_reason": self.stop_reason.unwrap_or("end_turn") },
                "usage": {
                    "output_tokens": self.output_tokens,
                    "cache_read_input_tokens": self.usage.cached_tokens,
                    "cache_creation_input_tokens": self.usage.cache_write_tokens,
                    // The uncached part only; see `usage_body`.
                    "input_tokens": self.usage.non_cached_input_tokens(),
                },
            }),
        )
    }

    fn message_stop_frame() -> SseFrame {
        SseFrame::named("message_stop", &json!({ "type": "message_stop" }))
    }

    fn zero_output_frame() -> SseFrame {
        SseFrame::named(
            "error",
            &json!({
                "type": "error",
                "error": { "type": "rate_limit_error", "message": ZERO_OUTPUT_MESSAGE },
                "retry_after": ZERO_OUTPUT_RETRY_AFTER,
            }),
        )
    }
}

impl ChunkGenerator for AnthropicStreamRenderer {
    type Event = SseFrame;

    fn generate(&mut self, chunk: &OutputChunk) -> Vec<SseFrame> {
        self.render(chunk)
    }

    fn end_events(&mut self) -> Vec<SseFrame> {
        self.end_events()
    }

    fn failure_frames(&mut self, error: &crate::core::Error) -> Vec<SseFrame> {
        vec![error_frame(error)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{OutputAccumulator, Usage};

    fn renderer() -> AnthropicStreamRenderer {
        AnthropicStreamRenderer::new("claude-sonnet-4-6", "msg_fixture")
    }

    fn frames(renderer: &mut AnthropicStreamRenderer, chunks: &[OutputChunk]) -> Vec<String> {
        chunks
            .iter()
            .flat_map(|chunk| renderer.render(chunk))
            .map(|frame| frame.as_str().to_owned())
            .collect()
    }

    fn name_of(frame: &str) -> &str {
        frame
            .strip_prefix("event: ")
            .and_then(|rest| rest.split_once('\n'))
            .map(|(name, _)| name)
            .unwrap_or("")
    }

    #[test]
    fn the_first_output_opens_the_message_and_a_block() {
        let mut renderer = renderer();
        let frames = frames(&mut renderer, &[OutputChunk::Text { text: "hi".to_owned() }]);
        let names: Vec<&str> = frames.iter().map(|frame| name_of(frame)).collect();
        assert_eq!(
            names,
            vec!["message_start", "content_block_start", "content_block_delta"]
        );
        assert!(renderer.started());
        assert!(
            frames[2].contains(r#""delta":{"type":"text_delta","text":"hi"}"#),
            "{}",
            frames[2]
        );
    }

    #[test]
    fn signal_only_chunks_leave_the_stream_unstarted() {
        let mut renderer = renderer();
        let frames = frames(
            &mut renderer,
            &[
                OutputChunk::Usage(Usage {
                    prompt_tokens: 4,
                    ..Usage::default()
                }),
                OutputChunk::Finish {
                    reason: FinishReason::Stop,
                },
            ],
        );
        assert!(frames.is_empty());
        assert!(
            !renderer.started(),
            "a status is still possible while nothing has been written"
        );
    }

    #[test]
    fn a_thinking_block_is_signed_when_it_closes() {
        let mut renderer = renderer();
        frames(&mut renderer, &[OutputChunk::Reasoning { text: "why".to_owned() }]);
        let closing = frames(
            &mut renderer,
            &[OutputChunk::Text {
                text: "answer".to_owned(),
            }],
        );
        let names: Vec<&str> = closing.iter().map(|frame| name_of(frame)).collect();
        assert_eq!(
            names,
            vec![
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta"
            ]
        );
        assert!(closing[0].contains(r#""type":"signature_delta""#), "{}", closing[0]);
        assert!(closing[0].contains(&fake_thinking_signature("why")), "{}", closing[0]);
    }

    #[test]
    fn a_tool_call_is_its_own_block_and_weighs_twenty_tokens() {
        let mut renderer = renderer();
        frames(&mut renderer, &[OutputChunk::Text { text: "a".to_owned() }]);
        let call = frames(
            &mut renderer,
            &[OutputChunk::ToolCall {
                id: Some("toolu_1".to_owned()),
                name: "search".to_owned(),
                arguments: r#"{"q":1}"#.to_owned(),
            }],
        );
        let names: Vec<&str> = call.iter().map(|frame| name_of(frame)).collect();
        assert_eq!(
            names,
            vec![
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop"
            ],
            "the open text block is closed before the call opens"
        );
        assert!(
            call[1].contains(r#""content_block":{"type":"tool_use","id":"toolu_1","name":"search","input":{}}"#),
            "{}",
            call[1]
        );

        let end = renderer.end_events();
        let usage = end[0].as_str();
        assert!(
            usage.contains(r#""output_tokens":21"#),
            "21 = one text token plus twenty for the call: {usage}"
        );
    }

    #[test]
    fn a_nameless_tool_call_gets_a_positional_id() {
        let mut renderer = renderer();
        let call = frames(
            &mut renderer,
            &[OutputChunk::ToolCall {
                id: None,
                name: "search".to_owned(),
                arguments: "{}".to_owned(),
            }],
        );
        assert!(call[1].contains(r#""id":"toolu_0""#), "{}", call[1]);
    }

    #[test]
    fn the_terminal_usage_counts_only_the_uncached_input() {
        let mut renderer = renderer();
        frames(&mut renderer, &[OutputChunk::Text { text: "a".to_owned() }]);
        frames(
            &mut renderer,
            &[
                OutputChunk::Usage(Usage {
                    prompt_tokens: 7_653,
                    completion_tokens: 3,
                    cached_tokens: 7_568,
                    no_cache_tokens: Some(85),
                    ..Usage::default()
                }),
                OutputChunk::Finish {
                    reason: FinishReason::ToolCalls,
                },
            ],
        );
        let end = renderer.end_events();
        // The open text block is closed first, then the terminal pair.
        assert_eq!(name_of(end[0].as_str()), "content_block_stop");
        assert!(end[1].as_str().contains(r#""stop_reason":"tool_use""#), "{}", end[1]);
        assert!(end[1].as_str().contains(r#""input_tokens":85"#), "{}", end[1]);
        assert!(
            end[1].as_str().contains(r#""cache_read_input_tokens":7568"#),
            "{}",
            end[1]
        );
        assert_eq!(name_of(end[2].as_str()), "message_stop");
    }

    #[test]
    fn a_stream_with_no_output_closes_on_an_error() {
        let mut renderer = renderer();
        frames(&mut renderer, &[OutputChunk::Text { text: "a".to_owned() }]);
        let mut quiet = renderer.clone();
        quiet.output_tokens = 0;
        let end = quiet.end_events();
        // The open block is closed, then the failure is reported in place of the
        // terminal pair.
        assert_eq!(end.len(), 2);
        let frame = end[1].as_str();
        assert_eq!(name_of(frame), "error");
        assert!(frame.contains(r#""type":"rate_limit_error""#), "{frame}");
        assert!(frame.contains(r#""retry_after":10"#), "{frame}");
    }

    #[test]
    fn a_failed_stream_is_not_closed() {
        let mut renderer = renderer();
        frames(
            &mut renderer,
            &[OutputChunk::Text {
                text: "partial".to_owned(),
            }],
        );
        frames(
            &mut renderer,
            &[OutputChunk::UpstreamError {
                error: Error::upstream("boom"),
            }],
        );
        assert!(renderer.has_error());
        assert!(
            renderer.end_events().is_empty(),
            "a terminal event after an error would hide it from downstream loops"
        );
    }

    #[test]
    fn the_error_payload_is_flat_and_ordered() {
        let error = Error::from_upstream(429, Some("RATE_LIMITED".to_owned()), "<429> slow down");
        assert_eq!(
            error_body(&error).to_string(),
            r#"{"type":"error","error":{"message":"<429> slow down","type":"rate_limit_error","code":"RATE_LIMITED"}}"#
        );
        assert_eq!(
            error_frame(&error).as_str(),
            format!("event: error\ndata: {}\n\n", error_body(&error))
        );
    }

    #[test]
    fn an_aborted_stream_ends_with_zero_usage() {
        let frames = AnthropicStreamRenderer::aborted_frames();
        let names: Vec<&str> = frames.iter().map(|frame| name_of(frame.as_str())).collect();
        assert_eq!(names, vec!["message_delta", "message_stop"]);
        assert!(
            frames[0]
                .as_str()
                .contains(r#""usage":{"output_tokens":0,"input_tokens":0,"cache_read_input_tokens":0}"#),
            "{}",
            frames[0]
        );
    }

    #[test]
    fn a_non_streaming_body_keeps_the_client_key_order() {
        let mut accumulator = OutputAccumulator::new();
        accumulator.push(&OutputChunk::Reasoning { text: "why".to_owned() });
        accumulator.push(&OutputChunk::Text {
            text: "answer".to_owned(),
        });
        accumulator.push(&OutputChunk::ToolCall {
            id: Some("toolu_1".to_owned()),
            name: "search".to_owned(),
            arguments: r#"{"q":"x"}"#.to_owned(),
        });
        accumulator.push(&OutputChunk::Finish {
            reason: FinishReason::ToolCalls,
        });
        let response = accumulator.into_response("msg_1", "claude-sonnet-4-6", 1_700_000_000);
        let body = message_body(
            &ResponseMeta {
                id: "msg_1".to_owned(),
                model: "claude-sonnet-4-6".to_owned(),
                created: 1_700_000_000,
                completed: 1_700_000_001,
            },
            &response,
        );

        let raw = body.to_string();
        let keys = [
            "\"id\"",
            "\"type\"",
            "\"role\"",
            "\"model\"",
            "\"content\"",
            "\"stop_reason\"",
            "\"stop_sequence\"",
            "\"usage\"",
        ];
        let positions: Vec<usize> = keys.iter().map(|key| raw.find(key).expect(key)).collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{raw}");

        assert_eq!(body["stop_reason"], json!("tool_use"));
        assert_eq!(body["stop_sequence"], json!(null));
        assert_eq!(
            body["content"][0],
            json!({ "type": "thinking", "thinking": "why", "signature": fake_thinking_signature("why") })
        );
        // No output was reported, so the count is estimated from the content:
        // nine UTF-16 units over four, rounded up, plus twenty for the call.
        assert_eq!(body["usage"]["output_tokens"], json!(23));
        assert_eq!(body["usage"]["input_tokens"], json!(0));
    }

    #[test]
    fn a_non_streaming_body_reports_the_uncached_input() {
        let response = CanonicalResponse {
            id: "msg_1".to_owned(),
            model: "claude-sonnet-4-6".to_owned(),
            created: 0,
            content: vec![ContentBlock::text("answer")],
            finish_reason: FinishReason::Stop,
            usage: Usage {
                prompt_tokens: 7_653,
                completion_tokens: 12,
                cached_tokens: 7_568,
                no_cache_tokens: Some(85),
                ..Usage::default()
            },
        };
        let body = message_body(
            &ResponseMeta {
                id: "msg_1".to_owned(),
                model: "claude-sonnet-4-6".to_owned(),
                created: 0,
                completed: 0,
            },
            &response,
        );
        assert_eq!(
            body["usage"].to_string(),
            r#"{"input_tokens":85,"output_tokens":12,"cache_creation_input_tokens":0,"cache_read_input_tokens":7568}"#
        );
    }
}
