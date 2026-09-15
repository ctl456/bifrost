//! The contract every protocol adapter implements.

use bifrost_core::{CanonicalRequest, CanonicalResponse, ChunkGenerator, ConversionError};
use serde_json::Value;

use crate::sse::SseFrame;
use crate::warning::Decoded;

/// Facts the edge mints for a response, independent of protocol.
///
/// Generated rather than converted, so they are passed in instead of living in
/// the IR: the IR describes a conversation, not the identity of one delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseMeta {
    /// The completion id the client sees.
    pub id: String,
    /// The model name echoed back, which is the client's own unless it sent none.
    pub model: String,
    /// Unix seconds.
    pub created: i64,
    /// Unix seconds; when the delivery was finished.
    ///
    /// Separate from [`Self::created`] because the Responses endpoint reports
    /// both, and a renderer that read the clock to fill the second one would
    /// produce different bytes on every run of the same fixture.
    pub completed: i64,
}

/// Knobs a conversion may consult that are not part of the client's payload.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConvertOptions {
    /// Turn a client's `prompt_cache_key` into a cache breakpoint when the client
    /// marked none of its own.
    ///
    /// Off by default because it changes the prompt prefix the provider caches
    /// against, which is an operator's call rather than a conversion detail.
    pub synthesize_cache_breakpoint: bool,
}

/// One public protocol, in both directions.
pub trait ProtocolAdapter: Send + Sync {
    /// The name used in configuration and logs, e.g. `openai-chat`.
    fn name(&self) -> &'static str;

    /// The model name a response echoes when the client named none.
    ///
    /// Belongs to the protocol rather than to the caller: each public endpoint
    /// answers with a name from its own model family, and a client that is told
    /// it talked to `deepseek/deepseek-v4-flash` after calling `/v1/messages`
    /// would break its own bookkeeping. What the *upstream* falls back to is a
    /// separate decision, owned by the wire adapter.
    fn default_model(&self) -> &'static str;

    /// Decode a client request body into the canonical IR.
    fn decode_request(
        &self,
        body: &[u8],
        options: &ConvertOptions,
    ) -> Result<Decoded<CanonicalRequest>, ConversionError>;

    /// Render a complete response body for a non-streaming request.
    ///
    /// The request is passed as well as the response because a protocol may
    /// answer with part of what it was asked: the Responses endpoint echoes the
    /// client's own `instructions`, `reasoning` and `tools`, which the IR cannot
    /// rebuild. Adapters that do not echo simply ignore it.
    fn render_response(&self, meta: &ResponseMeta, request: &CanonicalRequest, response: &CanonicalResponse) -> Value;

    /// Build the renderer for a streaming response.
    fn stream_renderer(&self, meta: &ResponseMeta) -> Box<dyn ChunkGenerator<Event = SseFrame> + Send>;

    /// Frames to emit when the client goes away mid-stream.
    ///
    /// The client is gone, so this is not for the client's benefit: it exists so
    /// a downstream observer that already saw bytes gets a well-formed ending
    /// instead of a truncated stream.
    fn aborted_frames(&self, meta: &ResponseMeta) -> Vec<SseFrame>;
}

/// Every protocol this build serves.
#[must_use]
pub fn registry() -> Vec<Box<dyn ProtocolAdapter>> {
    vec![
        Box::new(crate::openai::chat::ChatCompletions),
        Box::new(crate::openai::responses::Responses),
        Box::new(crate::anthropic::AnthropicMessages),
    ]
}
