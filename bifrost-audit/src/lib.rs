//! Evidence-preserving audit storage.
//!
//! Two independent pieces:
//!
//! - [`ArchiveStore`] keeps original bytes under their SHA-256, so any claim
//!   about them can be re-checked byte-for-byte later.
//! - [`Journal`] records what the gateway decided, as append-only JSONL.
//!
//! Both exist because transformations here are lossy by design: a reducer or a
//! projection may summarize, but the original must remain retrievable and
//! verifiable. If verification fails, the caller falls back to the original
//! rather than shipping a half-transformed result.
//!
//! [`prune`] is the one operation here that destroys something, and it destroys
//! only the archive — the journal it is told about is never touched. What a pass
//! removed it records in that journal, so a reader who finds a digest with no bytes
//! behind it can tell a pruned blob from a broken writer.

#![forbid(unsafe_code)]

mod archive;
mod journal;
mod retention;
mod secret;

pub use archive::{ArchiveRef, ArchiveStore, Held, count_lines, digest_of, is_digest_name};
pub use journal::{Journal, digests_in};
pub use retention::{Pruned, Retention, prune};
pub use secret::contains_likely_secret;

/// Archive kind for the canonical inbound request.
pub const KIND_REQUEST: &str = "request";
/// Archive kind for the complete upstream response body.
pub const KIND_UPSTREAM_RESPONSE: &str = "upstream-response";
/// Archive kind for a raw upstream stream event.
pub const KIND_UPSTREAM_EVENT: &str = "upstream-event";
/// Journal kind for one pass of archive retention.
pub const KIND_RETENTION: &str = "retention";
