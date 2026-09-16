//! Randomness the wire layer needs.
//!
//! Session ids and trace ids must be unique, but they are not secrets: they are
//! correlation handles. This is deliberately a small injectable surface so tests
//! can make generation deterministic instead of asserting on random output.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

/// A source of unpredictable bytes.
pub trait Entropy: Send + Sync {
    /// Fill `buffer` completely.
    fn fill(&self, buffer: &mut [u8]);
}

/// Entropy mixed from the clock, the process id and a per-call counter.
///
/// This produces unique values without pulling in a cryptographic RNG. It must
/// not be used for anything that needs to resist prediction.
#[derive(Debug, Default)]
pub struct SystemEntropy {
    counter: AtomicU64,
}

impl SystemEntropy {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Entropy for SystemEntropy {
    fn fill(&self, buffer: &mut [u8]) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let counter = self.counter.fetch_add(1, Ordering::Relaxed);

        let mut hasher = Sha256::new();
        hasher.update(nanos.to_le_bytes());
        hasher.update(std::process::id().to_le_bytes());
        hasher.update(counter.to_le_bytes());
        let mut block: [u8; 32] = hasher.finalize().into();

        let mut filled = 0;
        let mut round = 0u64;
        while filled < buffer.len() {
            if round > 0 {
                let mut hasher = Sha256::new();
                hasher.update(block);
                hasher.update(round.to_le_bytes());
                block = hasher.finalize().into();
            }
            let take = (buffer.len() - filled).min(block.len());
            buffer[filled..filled + take].copy_from_slice(&block[..take]);
            filled += take;
            round += 1;
        }
    }
}

/// A reproducible byte stream, for tests.
#[derive(Debug)]
pub struct SequenceEntropy {
    seed: u8,
    counter: AtomicU64,
}

impl SequenceEntropy {
    #[must_use]
    pub fn new(seed: u8) -> Self {
        Self {
            seed,
            counter: AtomicU64::new(0),
        }
    }
}

impl Entropy for SequenceEntropy {
    fn fill(&self, buffer: &mut [u8]) {
        let round = self.counter.fetch_add(1, Ordering::Relaxed) as u8;
        for (index, byte) in buffer.iter_mut().enumerate() {
            *byte = self.seed.wrapping_add(round).wrapping_add(index as u8);
        }
    }
}
