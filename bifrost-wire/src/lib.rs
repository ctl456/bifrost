//! Upstream wire dialects.
//!
//! A [`WireAdapter`] turns a canonical request into a fully-formed upstream
//! request. Dialect versions are separate implementations selected by
//! configuration, so a protocol change is additive.

#![forbid(unsafe_code)]

pub mod adapter;
pub mod cc;
pub mod context;
pub mod entropy;
pub mod request;
pub mod util;

pub use adapter::{SUPPORTED, WireAdapter, adapter_for};
pub use context::WireContext;
pub use entropy::{Entropy, SequenceEntropy, SystemEntropy};
pub use request::WireRequest;
pub use util::{Traceparent, is_uuid, slugify_path, today_utc, uuid_v4};
