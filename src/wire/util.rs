//! Wire-level value construction shared by adapters.

use std::fmt;

use crate::wire::entropy::Entropy;

/// A W3C Trace Context `traceparent` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Traceparent {
    trace_id: [u8; 16],
    parent_id: [u8; 8],
}

impl Traceparent {
    /// Generate a sampled trace context.
    #[must_use]
    pub fn generate(entropy: &dyn Entropy) -> Self {
        let mut trace_id = [0u8; 16];
        let mut parent_id = [0u8; 8];
        entropy.fill(&mut trace_id);
        entropy.fill(&mut parent_id);
        Self { trace_id, parent_id }
    }

    #[must_use]
    pub fn trace_id(&self) -> String {
        hex::encode(self.trace_id)
    }

    #[must_use]
    pub fn parent_id(&self) -> String {
        hex::encode(self.parent_id)
    }
}

impl fmt::Display for Traceparent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "00-{}-{}-01", self.trace_id(), self.parent_id())
    }
}

/// A random RFC 4122 version 4 UUID.
#[must_use]
pub fn uuid_v4(entropy: &dyn Entropy) -> String {
    let mut bytes = [0u8; 16];
    entropy.fill(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{}-{}-{}-{}-{}",
        hex::encode(&bytes[0..4]),
        hex::encode(&bytes[4..6]),
        hex::encode(&bytes[6..8]),
        hex::encode(&bytes[8..10]),
        hex::encode(&bytes[10..16])
    )
}

/// Whether `value` is a canonical 8-4-4-4-12 hex UUID.
#[must_use]
pub fn is_uuid(value: &str) -> bool {
    const SHAPE: [usize; 5] = [8, 4, 4, 4, 12];
    let mut parts = value.split('-');
    for expected in SHAPE {
        match parts.next() {
            Some(part) => {
                if part.len() != expected || !part.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return false;
                }
            }
            None => return false,
        }
    }
    parts.next().is_none()
}

/// Slugify a path the way the official CLI does.
///
/// Punctuation runs collapse to a single `-`, leading and trailing dashes are
/// removed, and an empty result becomes `root`. The slug is derived from the
/// same directory reported as the working directory, so the two cannot disagree.
#[must_use]
pub fn slugify_path(path: &str) -> String {
    let mut slug = String::with_capacity(path.len());
    let mut pending_dash = false;
    for character in path.chars() {
        // Lowercase first, exactly as the client does: `A` and `a` slugify the same.
        let lowered = character.to_lowercase().next().unwrap_or(character);
        if lowered.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            pending_dash = false;
            slug.push(lowered);
        } else {
            pending_dash = true;
        }
    }
    if slug.is_empty() { "root".to_owned() } else { slug }
}

/// Today's date in UTC, as `YYYY-MM-DD`.
#[must_use]
pub fn today_utc() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!("{:04}-{:02}-{:02}", now.year(), u8::from(now.month()), now.day())
}
