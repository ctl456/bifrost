//! Tool definitions and selection, independent of any protocol's spelling.

use serde::{Deserialize, Serialize};

/// A callable function the model may invoke.
///
/// `parameters` stays a raw JSON Schema value: Bifrost forwards it verbatim
/// rather than modelling every keyword, and no supported protocol requires
/// Bifrost to interpret it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

impl ToolDefinition {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            parameters: None,
        }
    }
}

/// How the model should pick tools.
///
/// Anthropic's `any` and OpenAI's `required` mean the same thing and both land
/// on [`ToolChoice::Required`]; a forced tool lands on [`ToolChoice::Named`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
    Required,
    Named {
        name: String,
    },
}

impl ToolChoice {
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self::Named { name: name.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_definition_skips_absent_optional_fields() {
        let tool = ToolDefinition::new("get_weather");
        assert_eq!(
            serde_json::to_value(&tool).expect("serialize"),
            json!({ "name": "get_weather" })
        );
    }

    #[test]
    fn tool_choice_defaults_to_auto() {
        assert_eq!(ToolChoice::default(), ToolChoice::Auto);
    }

    #[test]
    fn named_tool_choice_round_trips() {
        let choice = ToolChoice::named("apply_patch");
        let value = serde_json::to_value(&choice).expect("serialize");
        assert_eq!(value, json!({ "mode": "named", "name": "apply_patch" }));
        assert_eq!(
            serde_json::from_value::<ToolChoice>(value).expect("deserialize"),
            choice
        );
    }
}
