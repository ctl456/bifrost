//! Normalized content blocks.

use serde::{Deserialize, Serialize};

/// A prompt-cache breakpoint.
///
/// Anthropic and OpenAI spell this the same way (`{"type":"ephemeral"}`), so the
/// block carries it verbatim instead of re-deriving it per protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub kind: String,
}

impl CacheControl {
    /// The only breakpoint kind either protocol currently defines.
    pub const EPHEMERAL: &'static str = "ephemeral";

    #[must_use]
    pub fn ephemeral() -> Self {
        Self {
            kind: Self::EPHEMERAL.to_owned(),
        }
    }

    /// Read a breakpoint a client wrote into its payload.
    ///
    /// Any marker counts as a breakpoint: a value sitting in a position reserved
    /// for one is a request for a cache boundary, whatever its shape, and every
    /// protocol this build speaks spells that request by presence. A recognized
    /// `type` is kept rather than normalized, because the kinds differ in how
    /// long the provider keeps the prefix, and replacing the client's choice
    /// would silently change how much of the prompt stays cached.
    #[must_use]
    pub fn from_json(value: &serde_json::Value) -> Self {
        let kind = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .filter(|kind| !kind.is_empty())
            .unwrap_or(Self::EPHEMERAL);
        Self { kind: kind.to_owned() }
    }
}

/// Where an image came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageSource {
    /// Inline base64 payload, as sent by both OpenAI `data:` URLs and Anthropic
    /// `base64` sources.
    Base64 { media_type: String, data: String },
    /// An external URL the upstream is expected to fetch.
    ///
    /// `media_type` is only set for a `data:` URL this build could not split
    /// into a payload, where it is the only way to keep the client's declared
    /// type on the wire.
    Url {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
    },
}

impl ImageSource {
    /// Classify an image URL.
    ///
    /// Both protocols spell an inline image as a base64 `data:` URL and both put
    /// the media type in a field of its own, so the split happens here and not in
    /// each adapter. It is derived from the URL alone because the URL is where
    /// the client already stated the type.
    #[must_use]
    pub fn from_url(url: impl Into<String>) -> Self {
        let url = url.into();
        match split_data_url(&url) {
            Some((media_type, data)) => Self::Base64 {
                media_type: media_type.to_owned(),
                data: data.to_owned(),
            },
            // A `data:` URL this build cannot split keeps its declared type
            // attached, which is the only place left to keep it.
            None => Self::Url {
                media_type: data_url_media_type(&url),
                url,
            },
        }
    }
}

/// Split a base64 `data:` URL into its media type and payload.
///
/// `None` for anything else, including a `data:` URL with no base64 marker: that
/// one is still an inline image, but its payload is not a base64 string and the
/// caller has to decide what to do with it.
fn split_data_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let (media_type, encoding) = meta.split_once(';')?;
    if !encoding.eq_ignore_ascii_case("base64") {
        return None;
    }
    Some((media_type, data))
}

/// The media type a `data:` URL declares, if it declares one.
fn data_url_media_type(url: &str) -> Option<String> {
    let rest = url.strip_prefix("data:")?;
    let (meta, _) = rest.split_once(',')?;
    let media_type = meta.split(';').next()?;
    (!media_type.is_empty()).then(|| media_type.to_owned())
}

/// One normalized piece of message content.
///
/// Reasoning is kept as an explicit block rather than folded into text: the
/// upstream rejects histories whose thinking was dropped, and the ordering
/// (`reasoning`, `text`, `tool_use`) is part of the wire contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Image {
        source: ImageSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Reasoning {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        /// The tool's name, when the client supplied it.
        ///
        /// Some clients only send the call id on a tool result; the wire encoder
        /// falls back to resolving the name from the matching tool call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    /// A part this build does not model, carried through exactly as the client
    /// wrote it.
    ///
    /// The original forwards what it does not recognize, and dropping it would
    /// be the worse failure: the user's payload would vanish from the prompt with
    /// nothing to show for it, where forwarding at least lets the upstream — the
    /// only party that can know what the part means — answer for it. Modelling it
    /// here would mean inventing a shape, so it is kept as it arrived.
    Opaque {
        value: serde_json::Value,
    },
}

impl ContentBlock {
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            cache_control: None,
        }
    }

    #[must_use]
    pub fn reasoning(text: impl Into<String>) -> Self {
        Self::Reasoning { text: text.into() }
    }

    /// A part to hand to the upstream without interpreting it.
    #[must_use]
    pub fn opaque(value: serde_json::Value) -> Self {
        Self::Opaque { value }
    }

    /// The text of a plain text block, if this is one.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text, .. } => Some(text),
            _ => None,
        }
    }

    /// Whether the client asked the provider to cache up to this block.
    #[must_use]
    pub fn cache_control(&self) -> Option<&CacheControl> {
        match self {
            Self::Text { cache_control, .. } | Self::Image { cache_control, .. } => cache_control.as_ref(),
            _ => None,
        }
    }

    #[must_use]
    pub fn is_cache_breakpoint(&self) -> bool {
        self.cache_control().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_block_round_trips_without_cache_control() {
        let block = ContentBlock::text("hi");
        let value = serde_json::to_value(&block).expect("serialize");
        assert_eq!(value, json!({ "type": "text", "text": "hi" }));

        let back: ContentBlock = serde_json::from_value(value).expect("deserialize");
        assert_eq!(back, block);
    }

    #[test]
    fn cache_control_is_preserved_verbatim() {
        let block = ContentBlock::Text {
            text: "cached".to_owned(),
            cache_control: Some(CacheControl::ephemeral()),
        };
        let value = serde_json::to_value(&block).expect("serialize");
        assert_eq!(
            value,
            json!({ "type": "text", "text": "cached", "cache_control": { "type": "ephemeral" } })
        );
        assert!(block.is_cache_breakpoint());
    }

    #[test]
    fn base64_image_source_round_trips() {
        let block = ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".to_owned(),
                data: "AAA=".to_owned(),
            },
            cache_control: None,
        };
        let value = serde_json::to_value(&block).expect("serialize");
        assert_eq!(
            value,
            json!({ "type": "image", "source": { "kind": "base64", "media_type": "image/png", "data": "AAA=" } })
        );

        let remote = ContentBlock::Image {
            source: ImageSource::Url {
                url: "https://example.com/a.png".to_owned(),
                media_type: None,
            },
            cache_control: None,
        };
        assert_eq!(
            serde_json::to_value(&remote).expect("serialize"),
            json!({ "type": "image", "source": { "kind": "url", "url": "https://example.com/a.png" } })
        );
    }

    #[test]
    fn an_inline_image_splits_into_a_media_type_and_a_payload() {
        assert_eq!(
            ImageSource::from_url("data:image/jpeg;base64,AAAA"),
            ImageSource::Base64 {
                media_type: "image/jpeg".to_owned(),
                data: "AAAA".to_owned(),
            }
        );
    }

    #[test]
    fn a_remote_image_stays_a_url() {
        assert_eq!(
            ImageSource::from_url("https://example.test/a.png"),
            ImageSource::Url {
                url: "https://example.test/a.png".to_owned(),
                media_type: None,
            }
        );
    }

    #[test]
    fn a_data_url_without_a_base64_payload_keeps_its_declared_type() {
        assert_eq!(
            ImageSource::from_url("data:image/svg+xml,<svg/>"),
            ImageSource::Url {
                url: "data:image/svg+xml,<svg/>".to_owned(),
                media_type: Some("image/svg+xml".to_owned()),
            }
        );
        assert_eq!(
            ImageSource::from_url("data:,"),
            ImageSource::Url {
                url: "data:,".to_owned(),
                media_type: None,
            }
        );
    }

    #[test]
    fn an_opaque_block_keeps_the_payload_it_was_given() {
        let block = ContentBlock::opaque(json!({ "type": "video", "url": "x" }));
        let value = serde_json::to_value(&block).expect("serialize");
        assert_eq!(
            value,
            json!({ "type": "opaque", "value": { "type": "video", "url": "x" } })
        );
        assert!(block.as_text().is_none(), "an opaque block is not text");
        assert!(!block.is_cache_breakpoint());
    }

    #[test]
    fn tool_result_omits_false_is_error() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "t1".to_owned(),
            name: None,
            content: "ok".to_owned(),
            is_error: false,
        };
        let value = serde_json::to_value(&block).expect("serialize");
        assert_eq!(
            value,
            json!({ "type": "tool_result", "tool_use_id": "t1", "content": "ok", "is_error": false })
        );
    }
}
