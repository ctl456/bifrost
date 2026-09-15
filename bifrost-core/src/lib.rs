//! Canonical types shared by every Bifrost protocol adapter.
//!
//! Bifrost converts three public protocols (OpenAI Chat Completions, OpenAI
//! Responses, Anthropic Messages) into one intermediate representation and back.
//! Adapters never talk to each other directly: they only ever translate to and
//! from the types in this crate, which keeps the number of conversion paths
//! linear instead of quadratic.

#![forbid(unsafe_code)]

pub mod canonical;
pub mod content;
pub mod error;
pub mod stream;
pub mod tools;

pub use canonical::{
    CanonicalMessage, CanonicalRequest, CanonicalResponse, DEFAULT_MAX_TOKENS, FinishReason, InferenceParams,
    ReasoningEffort, Role, Usage,
};
pub use content::{CacheControl, ContentBlock, ImageSource};
pub use error::{
    ConversionError, DEFAULT_RETRY_AFTER_SECS, Error, ErrorBody, ErrorDetail, ErrorType, map_upstream_status,
};
pub use stream::{ChunkGenerator, OutputAccumulator, OutputChunk, ToolCall, tool_call_id};
pub use tools::{ToolChoice, ToolDefinition};
