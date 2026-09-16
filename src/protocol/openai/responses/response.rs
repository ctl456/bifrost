//! Canonical output → OpenAI Responses frames.
//!
//! Three things about this endpoint shape the code below:
//!
//! - **Every event carries a `sequence_number`**, counted from zero, and it is
//!   the second key in the payload. The count is per stream, so the renderer owns
//!   it: an event minted outside the renderer would not be numbered.
//! - **There are no frame-level sentinels.** A stream ends by closing, so
//!   [`ResponsesStreamRenderer::end_events`] is the only terminator and the
//!   protocol has nothing to send for a client that already left.
//! - **A truncated turn is `incomplete`, not `completed`.** `max_output_tokens`
//!   is reported as a status, so a client that retries does not believe it has
//!   the whole answer.
//!
//! The non-streaming body echoes part of the request back, which the IR cannot
//! rebuild: see [`CanonicalRequest::echo`].

use crate::core::{
    CanonicalRequest, CanonicalResponse, ChunkGenerator, ContentBlock, FinishReason, OutputChunk, Usage,
};
use serde_json::{Map, Value, json};

use crate::protocol::adapter::ResponseMeta;
use crate::protocol::sse::SseFrame;

/// Render a complete (non-streaming) response body.
#[must_use]
pub fn response_body(meta: &ResponseMeta, request: &CanonicalRequest, response: &CanonicalResponse) -> Value {
    let echo = request.echo.as_ref();
    let truncated = response.finish_reason == FinishReason::Length;
    let output = output_items(response);

    let mut body = Map::new();
    body.insert("id".to_owned(), json!(meta.id));
    body.insert("object".to_owned(), json!("response"));
    body.insert("created_at".to_owned(), json!(meta.created));
    body.insert("status".to_owned(), json!(status(truncated)));
    body.insert("completed_at".to_owned(), json!(meta.completed));
    // Both always present and null on success: a client branches on them, and an
    // absent key reads differently from a null one in most SDKs.
    body.insert("error".to_owned(), Value::Null);
    body.insert("incomplete_details".to_owned(), incomplete_details(truncated));
    // The endpoint answers with the request it was given; `input` is the one part
    // the original never fills in, so it is always empty here too.
    body.insert("input".to_owned(), json!([]));
    body.insert("instructions".to_owned(), echoed(echo, "instructions", Value::Null));
    body.insert(
        "max_output_tokens".to_owned(),
        echoed(echo, "max_output_tokens", Value::Null),
    );
    body.insert("model".to_owned(), json!(meta.model));
    body.insert("output".to_owned(), Value::Array(output));
    body.insert("output_text".to_owned(), json!(text_of(response)));
    body.insert("parallel_tool_calls".to_owned(), json!(true));
    body.insert("previous_response_id".to_owned(), Value::Null);
    body.insert("reasoning".to_owned(), echoed(echo, "reasoning", Value::Null));
    body.insert("store".to_owned(), json!(false));
    body.insert("temperature".to_owned(), echoed(echo, "temperature", json!(1)));
    body.insert("text".to_owned(), json!({ "format": { "type": "text" } }));
    body.insert("tool_choice".to_owned(), echoed(echo, "tool_choice", json!("auto")));
    body.insert("tools".to_owned(), echoed(echo, "tools", json!([])));
    body.insert("top_p".to_owned(), echoed(echo, "top_p", json!(1)));
    body.insert("truncation".to_owned(), json!("disabled"));
    body.insert("usage".to_owned(), usage_body(response.usage));
    body.insert("user".to_owned(), Value::Null);
    body.insert("metadata".to_owned(), json!({}));
    Value::Object(body)
}

/// A value from the client's echo sidecar, or the endpoint's default for it.
fn echoed(echo: Option<&Value>, key: &str, default: Value) -> Value {
    echo.and_then(|echo| echo.get(key))
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or(default)
}

/// The `output` array of a complete response.
fn output_items(response: &CanonicalResponse) -> Vec<Value> {
    let mut output = Vec::new();
    let mut index = 0;
    for block in &response.content {
        match block {
            ContentBlock::Reasoning { text } if !text.is_empty() => {
                output.push(json!({
                    "type": "reasoning",
                    "id": item_id("rs_", index),
                    "summary": [{ "type": "summary_text", "text": text }],
                }));
                index += 1;
            }
            ContentBlock::Text { text, .. } if !text.is_empty() => {
                output.push(json!({
                    "type": "message",
                    "id": item_id("msg_", index),
                    "status": "completed",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": text, "annotations": [] }],
                }));
                index += 1;
            }
            ContentBlock::ToolUse { id, name, input } => {
                output.push(json!({
                    "type": "function_call",
                    "id": item_id("fc_", index),
                    "call_id": id,
                    "name": name,
                    // Re-encoded rather than remembered: the non-streaming path
                    // has no raw argument string left to forward.
                    "arguments": input.to_string(),
                    "status": "completed",
                }));
                index += 1;
            }
            _ => {}
        }
    }
    output
}

/// The text of a complete response, as the endpoint reports it separately.
fn text_of(response: &CanonicalResponse) -> String {
    response.content.iter().filter_map(ContentBlock::as_text).collect()
}

/// An item's id.
///
/// Positional rather than random: the original mints a UUID here, which makes its
/// output unreproducible, and an id only has to be unique within the response it
/// names. A stable id means the same conversation renders the same bytes twice.
fn item_id(prefix: &str, index: usize) -> String {
    format!("{prefix}{index}")
}

fn status(truncated: bool) -> &'static str {
    if truncated { "incomplete" } else { "completed" }
}

fn incomplete_details(truncated: bool) -> Value {
    if truncated {
        json!({ "reason": "max_output_tokens" })
    } else {
        Value::Null
    }
}

/// Accounting as this endpoint reports it.
///
/// `input_tokens` is the *total*, cache hits included — the opposite of the
/// Anthropic endpoint, where `input_tokens` counts only the uncached part and the
/// cache counters are separate increments. The upstream already reports the total,
/// so it is forwarded rather than adjusted; subtracting here would under-report
/// every cached prompt.
fn usage_body(usage: Usage) -> Value {
    // A response with no output reports no input either: an empty turn read
    // nothing the client should be billed for.
    let billed = usage.completion_tokens != 0;
    let input = if billed { usage.prompt_tokens } else { 0 };
    let cached = if billed { usage.cached_tokens } else { 0 };
    json!({
        "input_tokens": input,
        "input_tokens_details": {
            "cached_tokens": cached,
            "cache_write_tokens": usage.cache_write_tokens,
        },
        "output_tokens": usage.completion_tokens,
        // Required by the schema and always zero: the upstream does not separate
        // the thinking it billed from the answer it billed.
        "output_tokens_details": { "reasoning_tokens": 0 },
        "total_tokens": input + usage.completion_tokens,
    })
}

/// Which kind of item is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    Message,
    Reasoning,
    FunctionCall,
}

/// The item being streamed.
#[derive(Debug, Clone)]
struct OpenItem {
    kind: ItemKind,
    index: usize,
    /// The item as it will be reported, mutated as the item fills up.
    item: Map<String, Value>,
    text: String,
}

/// Renders canonical chunks as Responses event frames.
#[derive(Debug, Clone)]
pub struct ResponsesStreamRenderer {
    model: String,
    id: String,
    created: i64,
    /// The number the next event carries.
    sequence: u64,
    /// Whether `response.created` has been sent, which is the same question as
    /// whether any bytes have.
    started: bool,
    current: Option<OpenItem>,
    output_index: usize,
    tool_call_index: usize,
    /// Every item that has closed, in the order the response reports them.
    done: Vec<Value>,
    /// Every text delta, concatenated: `response.output_text` covers the whole
    /// response rather than one item, so it outlives any single message item.
    text: String,
    usage: Usage,
    finish_reason: Option<FinishReason>,
    has_error: bool,
}

impl ResponsesStreamRenderer {
    #[must_use]
    pub fn new(model: impl Into<String>, id: impl Into<String>, created: i64) -> Self {
        Self {
            model: model.into(),
            id: id.into(),
            created,
            sequence: 0,
            started: false,
            current: None,
            output_index: 0,
            tool_call_index: 0,
            done: Vec::new(),
            text: String::new(),
            usage: Usage::default(),
            finish_reason: None,
            has_error: false,
        }
    }

    /// Whether anything has been written to the client yet.
    ///
    /// The HTTP layer reads this to keep the response uncommitted, so a request
    /// that fails or produces nothing can still be answered with a status the
    /// client will retry on.
    #[must_use]
    pub const fn started(&self) -> bool {
        self.started
    }

    /// Whether the upstream failed mid-stream.
    ///
    /// A stream that failed is never closed with a terminal event: downstream
    /// loops stop at the first one, so it would hide the failure.
    #[must_use]
    pub const fn has_error(&self) -> bool {
        self.has_error
    }

    /// Turn one canonical chunk into frames.
    pub fn render(&mut self, chunk: &OutputChunk) -> Vec<SseFrame> {
        match chunk {
            OutputChunk::Text { text } => {
                if text.is_empty() {
                    return Vec::new();
                }
                let mut frames = self.lead();
                if !self.current.as_ref().is_some_and(|open| open.kind == ItemKind::Message) {
                    frames.extend(self.open_item(ItemKind::Message, None));
                }
                self.text.push_str(text);
                let Some(open) = self.current.as_mut() else {
                    return frames;
                };
                open.text.push_str(text);
                let (item_id, index) = (open.item.get("id").cloned(), open.index);
                frames.push(self.event(
                    "response.output_text.delta",
                    json!({
                        "item_id": item_id,
                        "output_index": index,
                        "content_index": 0,
                        "delta": text,
                        "logprobs": [],
                    }),
                ));
                frames
            }
            OutputChunk::Reasoning { text } => {
                if text.is_empty() {
                    return Vec::new();
                }
                let mut frames = self.lead();
                if !self
                    .current
                    .as_ref()
                    .is_some_and(|open| open.kind == ItemKind::Reasoning)
                {
                    frames.extend(self.open_item(ItemKind::Reasoning, None));
                }
                let Some(open) = self.current.as_mut() else {
                    return frames;
                };
                open.text.push_str(text);
                let (item_id, index) = (open.item.get("id").cloned(), open.index);
                frames.push(self.event(
                    "response.reasoning_summary_text.delta",
                    json!({ "item_id": item_id, "output_index": index, "summary_index": 0, "delta": text }),
                ));
                frames
            }
            OutputChunk::ToolCall { id, name, arguments } => {
                let mut frames = self.lead();
                // A call always starts an item of its own, even when one is open:
                // a call cannot be a continuation of anything.
                let call_id = crate::core::tool_call_id(id.as_deref(), self.tool_call_index);
                self.tool_call_index += 1;
                frames.extend(self.open_item(ItemKind::FunctionCall, Some((&call_id, name))));
                let Some(open) = self.current.as_mut() else {
                    return frames;
                };
                // The arguments land in the item only now: the frame that opened
                // it reported them empty, and a client that replays the opening
                // frame must not see them twice.
                open.item.insert("arguments".to_owned(), json!(arguments));
                let (item_id, index) = (open.item.get("id").cloned(), open.index);
                frames.push(self.event(
                    "response.function_call_arguments.delta",
                    json!({ "item_id": item_id, "output_index": index, "delta": arguments }),
                ));
                frames
            }
            // Both are attached to the frames that close the stream.
            OutputChunk::Usage(usage) => {
                self.usage = *usage;
                Vec::new()
            }
            OutputChunk::Finish { reason } => {
                self.finish_reason = Some(*reason);
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
        if self.has_error || !self.started {
            return Vec::new();
        }
        let mut frames = self.close_item();
        let truncated = self.finish_reason == Some(FinishReason::Length);
        let mut response = match self.base_response(status(truncated)) {
            Value::Object(body) => body,
            _ => return frames,
        };
        // Rewritten in place, so the keys keep the positions the opening frames
        // used; `usage` is new and lands last, where the original leaves it.
        response.insert("output".to_owned(), Value::Array(self.done.clone()));
        response.insert("output_text".to_owned(), json!(self.text));
        response.insert("incomplete_details".to_owned(), incomplete_details(truncated));
        response.insert("usage".to_owned(), usage_body(self.usage));
        let name = if truncated {
            "response.incomplete"
        } else {
            "response.completed"
        };
        frames.push(self.event(name, json!({ "response": Value::Object(response) })));
        frames
    }

    /// The frame a mid-stream failure is reported with.
    ///
    /// Not a terminal `response.completed`: the turn did not complete, and a
    /// client that only watches for the terminal event would otherwise treat a
    /// failed response as a finished one.
    pub fn failure_frames(&mut self, message: &str) -> Vec<SseFrame> {
        if !self.started {
            return Vec::new();
        }
        let mut response = match self.base_response("failed") {
            Value::Object(body) => body,
            _ => return Vec::new(),
        };
        response.insert(
            "error".to_owned(),
            json!({ "code": "upstream_error", "message": message }),
        );
        vec![self.event("response.failed", json!({ "response": Value::Object(response) }))]
    }

    /// The frame an in-flight failure is reported with when no response body is
    /// possible.
    pub fn error_frame(&mut self, message: &str) -> SseFrame {
        self.event(
            "error",
            json!({ "code": Value::Null, "message": message, "param": Value::Null }),
        )
    }

    /// Nothing to emit for a client that disconnected.
    ///
    /// The other protocols end with a sentinel or a terminal pair, which an
    /// observer can still be handed after the client leaves. This one numbers
    /// every event, so a terminal event minted without the stream's state would
    /// restart at zero and read as a duplicate rather than as an ending — worse
    /// than the truncation it was meant to paper over.
    #[must_use]
    pub fn aborted_frames() -> Vec<SseFrame> {
        Vec::new()
    }

    /// The first two events of every stream, sent once.
    fn lead(&mut self) -> Vec<SseFrame> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        let response = self.base_response("in_progress");
        let created = self.event("response.created", json!({ "response": response }));
        let in_progress = self.event("response.in_progress", json!({ "response": response }));
        vec![created, in_progress]
    }

    /// Close whatever item is open, if any.
    fn close_item(&mut self) -> Vec<SseFrame> {
        let Some(mut open) = self.current.take() else {
            return Vec::new();
        };
        let mut frames = Vec::new();
        let item_id = open.item.get("id").cloned().unwrap_or(Value::Null);
        let index = open.index;
        match open.kind {
            ItemKind::Message => {
                frames.push(self.event(
                    "response.output_text.done",
                    json!({
                        "item_id": item_id,
                        "output_index": index,
                        "content_index": 0,
                        "text": open.text,
                        "logprobs": [],
                    }),
                ));
                let part = json!({ "type": "output_text", "text": open.text, "annotations": [] });
                frames.push(self.event(
                    "response.content_part.done",
                    json!({
                        "item_id": item_id,
                        "output_index": index,
                        "content_index": 0,
                        "part": part,
                    }),
                ));
                open.item.insert("content".to_owned(), json!([part]));
                open.item.insert("status".to_owned(), json!("completed"));
            }
            ItemKind::Reasoning => {
                frames.push(self.event(
                    "response.reasoning_summary_text.done",
                    json!({
                        "item_id": item_id,
                        "output_index": index,
                        "summary_index": 0,
                        "text": open.text,
                    }),
                ));
                let part = json!({ "type": "summary_text", "text": open.text });
                frames.push(self.event(
                    "response.reasoning_summary_part.done",
                    json!({
                        "item_id": item_id,
                        "output_index": index,
                        "summary_index": 0,
                        "part": part,
                    }),
                ));
                open.item.insert("summary".to_owned(), json!([part]));
                open.item.insert("status".to_owned(), json!("completed"));
            }
            ItemKind::FunctionCall => {
                frames.push(self.event(
                    "response.function_call_arguments.done",
                    json!({
                        "item_id": item_id,
                        "output_index": index,
                        "arguments": open.item.get("arguments").cloned().unwrap_or(json!("")),
                    }),
                ));
                open.item.insert("status".to_owned(), json!("completed"));
            }
        }
        let item = Value::Object(open.item);
        frames.push(self.event(
            "response.output_item.done",
            json!({ "output_index": index, "item": item }),
        ));
        self.done.push(item);
        frames
    }

    /// Open a new item, closing whatever was open first.
    /// Opens an item of `kind`, returning the frames that announce it.
    ///
    /// `call` carries the id and the name of a function call. Both belong to the
    /// frame that opens the item, because that frame is the only thing a client
    /// is given to associate the later argument deltas with — an id that arrives
    /// only in the closing frame arrives too late to name anything. `arguments`
    /// is the one field that is empty on purpose: it streams in afterwards, and
    /// a client replaying the opening frame must not see it twice.
    fn open_item(&mut self, kind: ItemKind, call: Option<(&str, &str)>) -> Vec<SseFrame> {
        let mut frames = self.close_item();
        let index = self.output_index;
        self.output_index += 1;
        let item = match kind {
            ItemKind::Message => json!({
                "type": "message",
                "id": item_id("msg_", index),
                "status": "in_progress",
                "role": "assistant",
                "content": [],
            }),
            ItemKind::Reasoning => json!({
                "type": "reasoning",
                "id": item_id("rs_", index),
                "summary": [],
                "status": "in_progress",
            }),
            ItemKind::FunctionCall => {
                let (call_id, name) = call.unwrap_or(("", ""));
                json!({
                    "type": "function_call",
                    "id": item_id("fc_", index),
                    "call_id": call_id,
                    "name": name,
                    "arguments": "",
                    "status": "in_progress",
                })
            }
        };
        let Value::Object(item) = item else {
            return frames;
        };
        let item_id = item.get("id").cloned().unwrap_or(Value::Null);
        frames.push(self.event(
            "response.output_item.added",
            json!({ "output_index": index, "item": Value::Object(item.clone()) }),
        ));
        self.current = Some(OpenItem {
            kind,
            index,
            item,
            text: String::new(),
        });
        match kind {
            ItemKind::Message => frames.push(self.event(
                "response.content_part.added",
                json!({
                    "item_id": item_id,
                    "output_index": index,
                    "content_index": 0,
                    "part": { "type": "output_text", "text": "", "annotations": [] },
                }),
            )),
            ItemKind::Reasoning => frames.push(self.event(
                "response.reasoning_summary_part.added",
                json!({
                    "item_id": item_id,
                    "output_index": index,
                    "summary_index": 0,
                    "part": { "type": "summary_text", "text": "" },
                }),
            )),
            ItemKind::FunctionCall => {}
        }
        frames
    }

    /// The response skeleton the streaming events carry.
    ///
    /// Deliberately not the full body: the streaming frames name the fields a
    /// client acts on and stop there, and the echoed request fields belong to the
    /// non-streaming body alone.
    fn base_response(&self, status: &str) -> Value {
        let mut body = Map::new();
        body.insert("id".to_owned(), json!(self.id));
        body.insert("object".to_owned(), json!("response"));
        body.insert("created_at".to_owned(), json!(self.created));
        body.insert("status".to_owned(), json!(status));
        body.insert("output".to_owned(), json!([]));
        body.insert("output_text".to_owned(), json!(""));
        body.insert("model".to_owned(), json!(self.model));
        body.insert("error".to_owned(), Value::Null);
        body.insert("incomplete_details".to_owned(), Value::Null);
        body.insert("parallel_tool_calls".to_owned(), json!(true));
        body.insert("previous_response_id".to_owned(), Value::Null);
        body.insert("store".to_owned(), json!(false));
        body.insert("tools".to_owned(), json!([]));
        body.insert("metadata".to_owned(), json!({}));
        Value::Object(body)
    }

    /// Build one frame, numbering it.
    fn event(&mut self, name: &str, payload: Value) -> SseFrame {
        let mut data = Map::new();
        data.insert("type".to_owned(), json!(name));
        data.insert("sequence_number".to_owned(), json!(self.sequence));
        self.sequence += 1;
        if let Value::Object(fields) = payload {
            data.extend(fields);
        }
        SseFrame::named(name, &Value::Object(data))
    }
}

impl ChunkGenerator for ResponsesStreamRenderer {
    type Event = SseFrame;

    fn generate(&mut self, chunk: &OutputChunk) -> Vec<SseFrame> {
        self.render(chunk)
    }

    fn end_events(&mut self) -> Vec<SseFrame> {
        self.end_events()
    }

    fn failure_frames(&mut self, error: &crate::core::Error) -> Vec<SseFrame> {
        self.failure_frames(&error.message)
    }
}
