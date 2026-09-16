//! The `/v1/responses` adapter.

pub mod convert;
pub mod response;
pub mod schema;

use crate::core::{CanonicalRequest, CanonicalResponse, ChunkGenerator, ConversionError};
use serde_json::Value;

use crate::protocol::adapter::{ConvertOptions, ProtocolAdapter, ResponseMeta};
use crate::protocol::sse::SseFrame;
use crate::protocol::warning::Decoded;

pub use response::ResponsesStreamRenderer;
pub use schema::ResponsesRequest;

/// The Responses protocol.
#[derive(Debug, Clone, Copy, Default)]
pub struct Responses;

/// The name this adapter answers to.
pub const NAME: &str = "openai-responses";

/// The model a Responses body reports when the client named none.
pub const DEFAULT_MODEL: &str = "deepseek/deepseek-v4-flash";

impl ProtocolAdapter for Responses {
    fn name(&self) -> &'static str {
        NAME
    }

    fn default_model(&self) -> &'static str {
        DEFAULT_MODEL
    }

    /// Decode a client request body.
    ///
    /// `options` has nothing to act on: the cache-breakpoint synthesis it
    /// controls is driven by OpenAI's `prompt_cache_key`, and this endpoint has
    /// no such field — a client that wants a cache boundary sends `instructions`
    /// and lets the wire dialect place the separators.
    fn decode_request(
        &self,
        body: &[u8],
        _options: &ConvertOptions,
    ) -> Result<Decoded<CanonicalRequest>, ConversionError> {
        convert::decode(body)
    }

    fn render_response(&self, meta: &ResponseMeta, request: &CanonicalRequest, response: &CanonicalResponse) -> Value {
        response::response_body(meta, request, response)
    }

    fn stream_renderer(&self, meta: &ResponseMeta) -> Box<dyn ChunkGenerator<Event = SseFrame> + Send> {
        Box::new(ResponsesStreamRenderer::new(
            meta.model.clone(),
            meta.id.clone(),
            meta.created,
        ))
    }

    fn aborted_frames(&self, _meta: &ResponseMeta) -> Vec<SseFrame> {
        ResponsesStreamRenderer::aborted_frames()
    }
}
