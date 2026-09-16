//! Command Code dialects.
//!
//! Each version is a separate type. Nothing here is shared between versions
//! except the canonical types they encode from.

mod decode;
mod v1531;

pub use decode::{CcDecoder, DecodedLine};
pub use v1531::CcV1531;
