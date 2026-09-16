//! Reporting lossy conversions without failing them.

use serde::{Deserialize, Serialize};

/// Something the adapter had to drop or reinterpret.
///
/// Carried out of band rather than logged at the point of discovery so the
/// caller decides where it goes — a journal, a log line, or an audit record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warning {
    /// Where in the payload the loss happened, e.g. `messages[2].content[1]`.
    pub path: String,
    /// What was dropped and why the IR cannot hold it.
    pub detail: String,
}

impl Warning {
    #[must_use]
    pub fn new(path: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            detail: detail.into(),
        }
    }
}

/// A converted value together with everything the conversion could not keep.
#[derive(Debug, Clone, PartialEq)]
pub struct Decoded<T> {
    pub value: T,
    pub warnings: Vec<Warning>,
}

impl<T> Decoded<T> {
    #[must_use]
    pub fn lossless(value: T) -> Self {
        Self {
            value,
            warnings: Vec::new(),
        }
    }

    pub fn warn(&mut self, path: impl Into<String>, detail: impl Into<String>) {
        self.warnings.push(Warning::new(path, detail));
    }

    /// Attach a warning and keep going, for use inside builder loops.
    pub fn with_warning(mut self, path: impl Into<String>, detail: impl Into<String>) -> Self {
        self.warn(path, detail);
        self
    }
}

impl<T> From<T> for Decoded<T> {
    fn from(value: T) -> Self {
        Self::lossless(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lossless_conversion_carries_no_warnings() {
        let decoded = Decoded::lossless(7_u8);
        assert_eq!(decoded.value, 7);
        assert!(decoded.warnings.is_empty());
        assert_eq!(Decoded::from(7_u8), decoded);
    }

    #[test]
    fn warnings_accumulate_in_order() {
        let mut decoded = Decoded::lossless(());
        decoded.warn("messages[0].content[3]", "unknown part type");
        decoded.warn("tools[1].name", "empty name");
        assert_eq!(
            decoded.warnings,
            vec![
                Warning::new("messages[0].content[3]", "unknown part type"),
                Warning::new("tools[1].name", "empty name"),
            ]
        );
    }
}
