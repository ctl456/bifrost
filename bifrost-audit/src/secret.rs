//! A cheap pre-flight check before text is written anywhere durable.
//!
//! This is a heuristic, not a guarantee: a false negative only means the caller
//! must still treat logs as sensitive.

/// Keys that imply the following text is a credential.
const MARKERS: [&str; 7] = [
    "api_key",
    "apikey",
    "authorization",
    "bearer",
    "access_token",
    "secret",
    "token",
];

/// How far past a marker an assignment may appear.
const LOOKAHEAD: usize = 32;

/// Whether `text` looks like it embeds a credential.
///
/// Mirrors the original proxy's rule: a marker key followed within
/// [`LOOKAHEAD`] bytes by a `=` or `:` assignment.
#[must_use]
pub fn contains_likely_secret(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    let haystack = lowered.as_bytes();
    MARKERS.iter().any(|marker| {
        let needle = marker.as_bytes();
        let mut from = 0;
        while let Some(offset) = find_from(haystack, needle, from) {
            let start = offset + needle.len();
            let end = (start + LOOKAHEAD).min(haystack.len());
            let window = &haystack[start..end];
            if window.contains(&b'=') || window.contains(&b':') {
                return true;
            }
            from = start;
        }
        false
    })
}

fn find_from(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|position| position + from)
}
