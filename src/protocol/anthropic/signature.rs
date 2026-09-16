//! Fabricated signatures for thinking blocks.
//!
//! Anthropic signs thinking blocks and a gateway cannot mint a signature that
//! verifies; clients nonetheless refuse to render a thinking block without one,
//! so the block is signed with something that is well-formed rather than valid.
//! The construction below satisfies every check that is actually performed: a
//! base64 payload whose first byte is `0x12`. The thinking text is folded in so
//! two blocks never share a signature, which is what a real signature promises
//! and what a client that de-duplicates them relies on.

use sha2::{Digest, Sha256};

/// Mixed into the digest when there is no thinking text at all.
const EMPTY_SEED: &str = "dsh-proxy-thinking";

/// A well-formed stand-in for a thinking block's signature.
#[must_use]
pub fn fake_thinking_signature(thinking: &str) -> String {
    let seed = if thinking.is_empty() { EMPTY_SEED } else { thinking };
    let digest = Sha256::digest(seed.as_bytes());
    let mut raw = Vec::with_capacity(digest.len() + 2);
    // Length-prefixed, which is what the first byte of a real signature encodes.
    raw.push(0x12);
    raw.push(digest.len() as u8);
    raw.extend_from_slice(&digest);
    base64(&raw)
}

/// The base64 alphabet, standard with padding.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// RFC 4648 base64.
///
/// Hand-rolled rather than pulled in: this is the only place the workspace needs
/// an encoder, and the whole of it is fifteen lines.
fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        let packed = u32::from(chunk[0]) << 16 | u32::from(second) << 8 | u32::from(third);
        out.push(char::from(ALPHABET[(packed >> 18) as usize & 0x3f]));
        out.push(char::from(ALPHABET[(packed >> 12) as usize & 0x3f]));
        out.push(if chunk.len() > 1 {
            char::from(ALPHABET[(packed >> 6) as usize & 0x3f])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(ALPHABET[packed as usize & 0x3f])
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn a_signature_is_a_length_prefixed_digest() {
        let signature = fake_thinking_signature("why");
        // 0x12 then a 32-byte digest is 34 bytes, which pads to 48 characters,
        // and the first six bits of 0x12 are 4 — the `E` a client looks for.
        assert_eq!(signature.len(), 48);
        assert!(signature.starts_with('E'), "{signature}");
        assert!(signature.ends_with("=="), "{signature}");
    }

    #[test]
    fn the_text_decides_the_signature() {
        assert_eq!(fake_thinking_signature("a"), fake_thinking_signature("a"));
        assert_ne!(fake_thinking_signature("a"), fake_thinking_signature("b"));
        assert_eq!(
            fake_thinking_signature(""),
            fake_thinking_signature(EMPTY_SEED),
            "an empty block is signed as the seed itself"
        );
    }
}
