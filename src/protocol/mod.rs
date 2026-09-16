//! Adapters for the public client protocols.
//!
//! Each adapter decodes one protocol into the canonical IR and renders the IR
//! back into that same protocol. Adapters never call each other, so the number
//! of conversion paths stays linear in the number of protocols.
//!
//! A conversion that cannot represent part of its input reports *what* it could
//! not represent instead of failing the request: a client that sends one exotic
//! content block should still get an answer, and the operator should still be
//! able to see that something was dropped.

#![forbid(unsafe_code)]

pub mod adapter;
pub mod anthropic;
pub mod openai;
pub mod sse;
pub mod warning;

pub use adapter::{ProtocolAdapter, ResponseMeta, registry};
pub use sse::SseFrame;
pub use warning::{Decoded, Warning};
