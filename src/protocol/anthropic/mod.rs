//! The `/v1/messages` adapter.

pub mod convert;
pub mod response;
pub mod schema;
pub mod signature;

use crate::core::{CanonicalRequest, CanonicalResponse, ChunkGenerator, ConversionError};
use serde_json::Value;

use crate::protocol::adapter::{ConvertOptions, ProtocolAdapter, ResponseMeta};
use crate::protocol::sse::SseFrame;
use crate::protocol::warning::Decoded;

pub use response::AnthropicStreamRenderer;
pub use schema::MessagesRequest;

/// The Anthropic Messages protocol.
#[derive(Debug, Clone, Copy, Default)]
pub struct AnthropicMessages;

/// The name this adapter answers to.
pub const NAME: &str = "anthropic-messages";

/// The model a Messages response reports when the client named none.
///
/// A name from this protocol's own family, so a client that logs what answered
/// it does not end up believing it spoke to an OpenAI model.
pub const DEFAULT_MODEL: &str = "claude-sonnet-4-6";

impl ProtocolAdapter for AnthropicMessages {
    fn name(&self) -> &'static str {
        NAME
    }

    fn default_model(&self) -> &'static str {
        DEFAULT_MODEL
    }

    /// Decode a client request body.
    ///
    /// `options` has nothing to act on here: the cache-breakpoint synthesis it
    /// controls is driven by OpenAI's `prompt_cache_key`, and this protocol
    /// names its cache boundaries with blocks instead. A breakpoint is never
    /// invented for a client that drew its own.
    fn decode_request(
        &self,
        body: &[u8],
        _options: &ConvertOptions,
    ) -> Result<Decoded<CanonicalRequest>, ConversionError> {
        convert::decode(body)
    }

    fn render_response(&self, meta: &ResponseMeta, _request: &CanonicalRequest, response: &CanonicalResponse) -> Value {
        response::message_body(meta, response)
    }

    fn stream_renderer(&self, meta: &ResponseMeta) -> Box<dyn ChunkGenerator<Event = SseFrame> + Send> {
        Box::new(AnthropicStreamRenderer::new(meta.model.clone(), meta.id.clone()))
    }

    fn aborted_frames(&self, _meta: &ResponseMeta) -> Vec<SseFrame> {
        AnthropicStreamRenderer::aborted_frames()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{CanonicalMessage, FinishReason, OutputChunk, Usage};
    use serde_json::json;

    fn adapter() -> AnthropicMessages {
        AnthropicMessages
    }

    #[test]
    fn the_adapter_answers_to_its_configured_name() {
        assert_eq!(adapter().name(), "anthropic-messages");
        assert_eq!(adapter().default_model(), "claude-sonnet-4-6");
    }

    #[test]
    fn a_request_round_trips_through_the_trait() {
        let body = serde_json::to_vec(&json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 1024,
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .expect("serialize");
        let decoded = adapter()
            .decode_request(&body, &ConvertOptions::default())
            .expect("decode");
        assert_eq!(decoded.value.messages, vec![CanonicalMessage::user("hi")]);
        assert_eq!(decoded.value.params.max_tokens, Some(1024));
    }

    #[test]
    fn a_response_round_trips_through_the_trait() {
        let response = CanonicalResponse {
            id: "msg_1".to_owned(),
            model: "claude-sonnet-4-6".to_owned(),
            created: 0,
            content: vec![crate::core::ContentBlock::text("hi")],
            finish_reason: FinishReason::Stop,
            usage: Usage {
                prompt_tokens: 3,
                completion_tokens: 1,
                ..Usage::default()
            },
        };
        let meta = ResponseMeta {
            id: "msg_1".to_owned(),
            model: "claude-sonnet-4-6".to_owned(),
            created: 0,
            completed: 0,
        };
        let request = CanonicalRequest::new(Vec::new());
        let body = adapter().render_response(&meta, &request, &response);
        assert_eq!(body["type"], json!("message"));
        assert_eq!(body["content"][0], json!({ "type": "text", "text": "hi" }));

        let mut renderer = adapter().stream_renderer(&meta);
        let frames = renderer.generate(&OutputChunk::Text { text: "hi".to_owned() });
        assert!(frames[0].as_str().starts_with("event: message_start"));
        assert!(!adapter().aborted_frames(&meta).is_empty());
    }
}
