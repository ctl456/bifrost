//! Content-addressed blob storage.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A handle to an archived blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveRef {
    /// Lowercase hex SHA-256 of the stored bytes.
    pub sha256: String,
    pub bytes: u64,
    pub lines: u64,
}

/// The SHA-256 of `bytes`, as lowercase hex.
#[must_use]
pub fn digest_of(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Count lines the way a reader would: a trailing newline does not open a new
/// line, and an empty body has no lines.
#[must_use]
pub fn count_lines(bytes: &[u8]) -> u64 {
    if bytes.is_empty() {
        return 0;
    }
    let newlines = bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
    if bytes.ends_with(b"\n") { newlines } else { newlines + 1 }
}

/// What the store has to say about a digest.
///
/// A digest is the only name the journal keeps for a turn's bytes, so a reader
/// holding a journal line holds a digest and nothing else. The three answers below
/// are the three things that can mean, and they are kept apart on purpose: no bytes
/// under the digest is a fact about the deployment's retention, and bytes that no
/// longer hash to it is a fact about the disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Held {
    /// Stored under this kind, and still hashing to the digest.
    Intact { kind: String, bytes: u64, lines: u64 },
    /// No bytes are stored under this digest.
    Gone,
    /// A blob is stored under this digest and no longer holds those bytes.
    Tampered { kind: String },
}

/// Whether a name is one this store can look up.
///
/// A reader that holds a name out of a journal has to decide what to do with the ones
/// that are not digests, and that decision has to be the same one the store makes, so
/// the rule lives here rather than in the reader.
///
/// The name is all that is checked. Reading every blob to verify it would be reading
/// the whole archive, and [`ArchiveStore::read`] refuses to serve a blob whose digest
/// does not match — so a blob that was tampered with is caught on the way out.
#[must_use]
pub fn is_digest_name(name: &str) -> bool {
    name.len() == 64 && name.chars().all(|c| c.is_ascii_digit() || matches!(c, 'a'..='f'))
}

/// A content-addressed store rooted at a directory.
///
/// Identical bytes are stored once. Nothing is ever overwritten, so a stored
/// handle stays valid for the lifetime of the directory.
#[derive(Debug, Clone)]
pub struct ArchiveStore {
    root: PathBuf,
}

impl ArchiveStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Archive `body` under `kind`, returning its handle.
    pub fn put(&self, kind: &str, body: &[u8]) -> io::Result<ArchiveRef> {
        validate_kind(kind)?;
        let reference = ArchiveRef {
            sha256: digest_of(body),
            bytes: body.len() as u64,
            lines: count_lines(body),
        };
        let path = self.path_for(kind, &reference);
        if path.exists() {
            return Ok(reference);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, body)?;
        Ok(reference)
    }

    /// Where a blob lives. The digest is the filename, so the path carries no
    /// caller-supplied text beyond the validated `kind`.
    #[must_use]
    pub fn path_for(&self, kind: &str, reference: &ArchiveRef) -> PathBuf {
        self.root.join(kind).join(&reference.sha256)
    }

    /// Read a blob back, verifying its digest first.
    pub fn read(&self, kind: &str, reference: &ArchiveRef) -> io::Result<Vec<u8>> {
        validate_kind(kind)?;
        let bytes = fs::read(self.path_for(kind, reference))?;
        if digest_of(&bytes) != reference.sha256 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("archived blob {} no longer matches its digest", reference.sha256),
            ));
        }
        Ok(bytes)
    }

    /// Look a digest up across every kind, and hash what is found.
    ///
    /// The journal records a digest without the kind it was stored under, so a reader
    /// that has only a digest has to look across all of them. Hashing the bytes is
    /// what makes the answer worth having: a file that no longer matches its name is
    /// reported as such instead of being handed over as the turn's bytes.
    pub fn held(&self, digest: &str) -> io::Result<Held> {
        if !is_digest_name(digest) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not a digest: {digest:?}"),
            ));
        }
        if !self.root.is_dir() {
            return Ok(Held::Gone);
        }
        for kind in fs::read_dir(&self.root)? {
            let kind = kind?;
            if !kind.file_type()?.is_dir() {
                continue;
            }
            let Some(name) = kind.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let path = kind.path().join(digest);
            if !path.is_file() {
                continue;
            }
            let bytes = fs::read(&path)?;
            if digest_of(&bytes) != digest {
                return Ok(Held::Tampered { kind: name });
            }
            return Ok(Held::Intact {
                kind: name,
                bytes: bytes.len() as u64,
                lines: count_lines(&bytes),
            });
        }
        Ok(Held::Gone)
    }

    /// Whether `quote` occurs byte-for-byte in the archived blob.
    ///
    /// An empty quote never counts as evidence, and a tampered blob never
    /// validates a claim.
    pub fn verify_quote(&self, kind: &str, reference: &ArchiveRef, quote: &[u8]) -> io::Result<bool> {
        if quote.is_empty() {
            return Ok(false);
        }
        let blob = match self.read(kind, reference) {
            Ok(blob) => blob,
            Err(error) if error.kind() == io::ErrorKind::InvalidData => return Ok(false),
            Err(error) => return Err(error),
        };
        Ok(contains(&blob, quote))
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.len() >= needle.len() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Reject kinds that could escape the store root.
fn validate_kind(kind: &str) -> io::Result<()> {
    let safe = !kind.is_empty()
        && kind
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if safe {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsafe archive kind: {kind:?}"),
        ))
    }
}
