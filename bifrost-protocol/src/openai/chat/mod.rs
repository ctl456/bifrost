//! The `/v1/chat/completions` adapter.

pub mod convert;
pub mod response;
pub mod schema;

use bifrost_core::{CanonicalRequest, CanonicalResponse, ChunkGenerator, ConversionError};
use serde_json::Value;

use crate::adapter::{ConvertOptions, ProtocolAdapter, ResponseMeta};
use crate::sse::SseFrame;
use crate::warning::Decoded;

pub use response::ChatStreamRenderer;
pub use schema::ChatCompletionRequest;

/// The Chat Completions protocol.
#[derive(Debug, Clone, Copy, Default)]
pub struct ChatCompletions;

/// The name this adapter answers to.
pub const NAME: &str = "openai-chat";

/// The model a Chat Completions response reports when the client named none.
pub const DEFAULT_MODEL: &str = "deepseek/deepseek-v4-flash";

impl ProtocolAdapter for ChatCompletions {
    fn name(&self) -> &'static str {
        NAME
    }

    fn default_model(&self) -> &'static str {
        DEFAULT_MODEL
    }

    fn decode_request(
        &self,
        body: &[u8],
        options: &ConvertOptions,
    ) -> Result<Decoded<CanonicalRequest>, ConversionError> {
        let mut decoded = convert::decode(body)?;
        if options.synthesize_cache_breakpoint {
            apply_prompt_cache_key(&mut decoded.value);
        }
        Ok(decoded)
    }

    fn render_response(&self, meta: &ResponseMeta, _request: &CanonicalRequest, response: &CanonicalResponse) -> Value {
        response::completion_body(meta, response)
    }

    fn stream_renderer(&self, meta: &ResponseMeta) -> Box<dyn ChunkGenerator<Event = SseFrame> + Send> {
        Box::new(ChatStreamRenderer::new(
            meta.model.clone(),
            meta.id.clone(),
            meta.created,
        ))
    }

    fn aborted_frames(&self, meta: &ResponseMeta) -> Vec<SseFrame> {
        ChatStreamRenderer::aborted_frames(&meta.model, &meta.id, meta.created)
    }
}

/// Place a cache breakpoint on the last system section.
///
/// Caching is prefix-based, and the system prompt is the first thing in every
/// request, so a client that names a cache key but marks no breakpoint is asking
/// for exactly this. A client that already marked one keeps its own choice: the
/// breakpoint closest to the end of the prefix is the one that matters, and only
/// the client knows where its stable prefix ends.
fn apply_prompt_cache_key(request: &mut CanonicalRequest) {
    if request.prompt_cache_key.as_deref().is_none_or(str::is_empty) {
        return;
    }
    if request.system.is_empty()
        || request
            .system
            .iter()
            .any(bifrost_core::ContentBlock::is_cache_breakpoint)
    {
        return;
    }
    let last = request.system.len() - 1;
    if let bifrost_core::ContentBlock::Text { cache_control, .. } = &mut request.system[last] {
        *cache_control = Some(bifrost_core::CacheControl::ephemeral());
    }
}
