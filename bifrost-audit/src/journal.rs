//! Append-only JSONL decision log.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// One line per decision.
///
/// Entries are machine-readable only; they never feed back into a model
/// context. `data` is nested under its own key so a field name inside it can
/// never shadow the envelope.
#[derive(Debug, Clone)]
pub struct Journal {
    path: PathBuf,
}

impl Journal {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one entry, creating parent directories as needed.
    pub fn append(&self, kind: &str, data: serde_json::Value) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let entry = serde_json::json!({
            "timestamp": now_rfc3339(),
            "kind": kind,
            "data": data,
        });
        let mut line = serde_json::to_string(&entry).map_err(io::Error::other)?;
        line.push('\n');

        let mut file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        file.write_all(line.as_bytes())
    }

    /// Read every entry back, skipping blanks.
    pub fn read_all(&self) -> io::Result<Vec<serde_json::Value>> {
        let text = fs::read_to_string(&self.path)?;
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).map_err(io::Error::other))
            .collect()
    }
}

/// Every digest an entry names, wherever inside it they appear.
///
/// A `sha256` field is the only name a journal line keeps for the bytes a turn was
/// made of, so this is how a reader gets from a line back to the archive. It walks the
/// entry rather than reading a fixed path because the two halves of a turn are nested
/// under their own keys and either can be absent.
#[must_use]
pub fn digests_in(entry: &serde_json::Value) -> std::collections::BTreeSet<String> {
    let mut digests = std::collections::BTreeSet::new();
    collect(entry, &mut digests);
    digests
}

fn collect(value: &serde_json::Value, digests: &mut std::collections::BTreeSet<String>) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, child) in fields {
                if key == "sha256"
                    && let Some(text) = child.as_str()
                {
                    digests.insert(text.to_owned());
                }
                collect(child, digests);
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|item| collect(item, digests)),
        _ => {}
    }
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A digest, as the writer spells it: lowercase hex, sixty-four characters.
    fn digest(seed: char) -> String {
        seed.to_string().repeat(64)
    }

    /// A turn line as this build writes one: both halves under their own keys, and
    /// either can be absent.
    #[test]
    fn every_sha256_a_nested_entry_holds_is_found() {
        let entry = json!({
            "timestamp": "2026-01-01T00:00:00Z",
            "kind": "turn",
            "data": {
                "model": "m",
                "request": { "sha256": digest('a'), "bytes": 3, "lines": 1 },
                "response": { "sha256": digest('b'), "bytes": 4, "lines": 1 },
            },
        });
        let found = digests_in(&entry);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(
            found.contains(&digest('a')) && found.contains(&digest('b')),
            "{found:?}"
        );
    }

    /// The halves are optional, so an entry that archived one of them must name one
    /// digest and not fail to answer.
    #[test]
    fn an_entry_that_archived_one_half_names_one_digest() {
        let entry = json!({ "kind": "turn", "data": { "response": { "sha256": digest('c') } } });
        assert_eq!(digests_in(&entry).into_iter().collect::<Vec<_>>(), vec![digest('c')]);
    }

    /// Identical bytes are stored once, so a turn whose two halves are the same bytes
    /// names one digest twice. The set is what keeps a reader from counting it twice.
    #[test]
    fn a_digest_named_twice_is_named_once() {
        let entry = json!({
            "data": {
                "request": { "sha256": digest('d') },
                "response": { "sha256": digest('d') },
            },
        });
        assert_eq!(digests_in(&entry).len(), 1);
    }

    #[test]
    fn an_entry_with_no_sha256_names_nothing() {
        assert!(digests_in(&json!({ "kind": "retention", "data": { "removed": 2 } })).is_empty());
        // A `sha256` that is not a string is not a name either: whatever it is, it is
        // not something the store could be holding bytes under.
        assert!(digests_in(&json!({ "data": { "sha256": 12 } })).is_empty());
        assert!(digests_in(&json!({ "data": { "sha256": null } })).is_empty());
        assert!(digests_in(&json!("a line that is not an object")).is_empty());
    }
}
