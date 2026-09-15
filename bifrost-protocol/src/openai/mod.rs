//! OpenAI-shaped protocols.
//!
//! Chat Completions and Responses are separate dialects that share vocabulary
//! (`tool_calls`, `image_url`, usage field names) but not message shape. The
//! shared pieces live here; each dialect owns its own schema and conversion.

pub mod chat;
pub mod responses;
pub mod shared;
