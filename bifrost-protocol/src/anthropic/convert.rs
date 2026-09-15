//! Anthropic Messages → canonical IR.
//!
//! A faithful port of the original proxy, because every branch here was learned
//! from a real request. Three of them are worth knowing before reading:
//!
//! - **A `tool_result` travels ahead of the text it arrived with.** Anthropic
//!   lets a client pack tool output and a follow-up question into one user turn;
//!   OpenAI wants the output immediately after the assistant turn that asked for
//!   it, so the results are queued and emitted first.
//! - **Reasoning leads the assistant turn.** The block order the upstream
//!   validates is `reasoning`, `text`, `tool_use` whatever order the client used.
//! - **An empty prompt is no prompt.** A `system` whose sections hold no text is
//!   dropped with its breakpoints, because a cache boundary on an empty prefix
//!   caches nothing.

use std::collections::HashMap;

use bifrost_core::{
    CanonicalMessage, CanonicalRequest, ContentBlock, ImageSource, InferenceParams, ReasoningEffort, Role, ToolChoice,
    ToolDefinition, tool_call_id,
};
use serde_json::{Value, json};

use crate::anthropic::schema::{Block, Message, MessageContent, MessagesRequest, SystemPrompt, Thinking};
use crate::warning::{Decoded, Warning};

/// Convert a decoded Messages body into the canonical IR.
pub fn decode(body: &[u8]) -> Result<Decoded<CanonicalRequest>, bifrost_core::ConversionError> {
    let wire: MessagesRequest = serde_json::from_slice(body)
        .map_err(|error| bifrost_core::ConversionError::bad_request(format!("invalid request body: {error}")))?;

    let mut decoded = Decoded::lossless(CanonicalRequest::new(Vec::new()));
    let request = &mut decoded.value;

    // An empty string is how this endpoint spells "unset"; passing it through
    // would send the upstream a model name that matches nothing.
    request.model = wire.model.clone().filter(|model| !model.is_empty());
    request.stream = wire.stream == Some(true);
    request.user = wire
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.user_id.clone())
        .filter(|user| !user.is_empty());
    request.system = system_sections(wire.system.as_ref(), &mut decoded.warnings);
    request.tools = tool_definitions(&wire);
    request.tool_choice = tool_choice(&wire, &mut decoded.warnings);
    request.params = params(&wire);
    request.messages = messages(&wire.messages, &mut decoded.warnings);

    Ok(decoded)
}

/// The prompt as sections.
///
/// A client that sends a string asked for one section. A client that sends
/// sections is asking for its cache breakpoints to survive, so the section shape
/// is preserved instead of being flattened into one string. The separator
/// *between* sections is not inserted here: it belongs to the wire dialect, and
/// doing it in both places would double it.
fn system_sections(system: Option<&SystemPrompt>, warnings: &mut Vec<Warning>) -> Vec<ContentBlock> {
    let sections = match system {
        None => return Vec::new(),
        Some(SystemPrompt::Text(text)) => {
            return if text.is_empty() {
                Vec::new()
            } else {
                vec![ContentBlock::text(text.clone())]
            };
        }
        Some(SystemPrompt::Sections(sections)) => sections.as_slice(),
        Some(SystemPrompt::Other(other)) => {
            warnings.push(Warning::new(
                "system",
                format!("`{other}` is not a prompt and was dropped"),
            ));
            return Vec::new();
        }
    };

    let mut texts = Vec::with_capacity(sections.len());
    for (index, section) in sections.iter().enumerate() {
        if section.kind.as_deref() != Some("text") {
            warnings.push(Warning::new(
                format!("system[{index}].type"),
                format!(
                    "`{}` is not a prompt section and was dropped",
                    section.kind.as_deref().unwrap_or("(none)")
                ),
            ));
            continue;
        }
        texts.push(section);
    }

    // Measured together rather than one by one: a lone section that holds a
    // breakpoint but no text is dropped along with the empty prompt it belongs
    // to, which is also what makes the upstream's placeholder system block
    // appear instead.
    if texts.iter().all(|section| text_of(section).is_empty()) {
        return Vec::new();
    }

    texts
        .into_iter()
        .filter_map(|section| {
            let text = text_of(section);
            let cache_control = breakpoint(section);
            // A section with neither text nor a breakpoint is not a section.
            (!text.is_empty() || cache_control.is_some()).then_some(ContentBlock::Text { text, cache_control })
        })
        .collect()
}

fn tool_definitions(wire: &MessagesRequest) -> Vec<ToolDefinition> {
    wire.tools
        .as_ref()
        .into_iter()
        .flatten()
        .map(|tool| ToolDefinition {
            name: tool.name.clone(),
            // The wire fills in the empty description and the empty schema; a
            // `None` here means exactly that, so the defaulting lives in one
            // place rather than two that can disagree.
            description: tool.description.clone(),
            parameters: tool.input_schema.clone(),
        })
        .collect()
}

/// `tool_choice`, in this protocol's spelling.
fn tool_choice(wire: &MessagesRequest, warnings: &mut Vec<Warning>) -> Option<ToolChoice> {
    let choice = wire.tool_choice.as_ref()?;
    match choice.kind.as_deref() {
        None | Some("auto") => Some(ToolChoice::Auto),
        Some("any") => Some(ToolChoice::Required),
        Some("none") => Some(ToolChoice::None),
        Some("tool") => match choice.name.clone().filter(|name| !name.is_empty()) {
            Some(name) => Some(ToolChoice::named(name)),
            None => {
                warnings.push(Warning::new(
                    "tool_choice.name",
                    "a forced tool choice without a name was treated as `auto`",
                ));
                Some(ToolChoice::Auto)
            }
        },
        Some(other) => {
            // Not carried either way: a choice this build cannot name is left
            // for the model to make, which is the same thing the original does
            // by dropping the field.
            warnings.push(Warning::new(
                "tool_choice.type",
                format!("unknown tool choice `{other}`; the upstream chose for itself"),
            ));
            None
        }
    }
}

fn params(wire: &MessagesRequest) -> InferenceParams {
    InferenceParams {
        // The endpoint requires a ceiling, so a zero is a client that meant to
        // send none, not a client asking for no output.
        max_tokens: wire.max_tokens.filter(|tokens| *tokens > 0),
        temperature: wire.temperature.filter(|value| value.is_finite()),
        top_p: wire.top_p.filter(|value| value.is_finite()),
        reasoning_effort: reasoning_effort(wire.thinking.as_ref()),
        thinking_budget_tokens: wire
            .thinking
            .as_ref()
            .and_then(|thinking| thinking.budget_tokens)
            .map(|tokens| tokens as u64),
        stop: wire.stop_sequences.clone().unwrap_or_default(),
    }
}

/// The effort level a thinking budget asks for.
///
/// A budget below every threshold is still a request to think, so it lands on
/// `low` rather than on nothing: sending no level at all would turn a request
/// for a little thinking into a request for none.
fn reasoning_effort(thinking: Option<&Thinking>) -> Option<ReasoningEffort> {
    let thinking = thinking?;
    match thinking.kind.as_deref() {
        Some("disabled" | "none") => None,
        Some("adaptive") => Some(ReasoningEffort::parse(thinking.effort.as_deref().unwrap_or("medium"))),
        _ => thinking
            .budget_tokens
            .map(|tokens| ReasoningEffort::from_thinking_budget(tokens as u64)),
    }
}

fn messages(messages: &[Message], warnings: &mut Vec<Warning>) -> Vec<CanonicalMessage> {
    let names = tool_names(messages);
    let mut out = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        match message.role.as_str() {
            "assistant" => out.push(assistant(index, message, warnings)),
            "user" => out.extend(user(index, message, &names, warnings)),
            // A turn of any other kind is dropped: the wire has no role for it,
            // and the upstream rejects a history it cannot order.
            other => warnings.push(Warning::new(
                format!("messages[{index}].role"),
                format!("`{other}` is not a turn the upstream accepts and was dropped"),
            )),
        }
    }
    out
}

/// `tool_use id → tool name`, gathered from the turns that made the calls.
///
/// A `tool_result` carries only the id, and the upstream wants a name with it,
/// so the mapping is built once rather than searched for per result.
fn tool_names(messages: &[Message]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for message in messages {
        for block in body_blocks(message) {
            if block.kind.as_deref() != Some("tool_use") {
                continue;
            }
            if let Some(id) = block.id.as_deref().filter(|id| !id.is_empty()) {
                names.insert(id.to_owned(), block.name.clone().unwrap_or_default());
            }
        }
    }
    names
}

fn assistant(index: usize, message: &Message, warnings: &mut Vec<Warning>) -> CanonicalMessage {
    let mut reasoning = String::new();
    let mut texts: Vec<ContentBlock> = Vec::new();
    let mut has_breakpoint = false;
    let mut calls: Vec<ContentBlock> = Vec::new();

    for (block_index, block) in body_blocks(message).iter().enumerate() {
        let path = format!("messages[{index}].content[{block_index}]");
        match block.kind.as_deref() {
            Some("text") => {
                let cache_control = breakpoint(block);
                has_breakpoint |= cache_control.is_some();
                texts.push(ContentBlock::Text {
                    text: text_of(block),
                    cache_control,
                });
            }
            // Every thinking block of a turn becomes the one reasoning string the
            // wire carries for it; the upstream compares this against the
            // history it was shown and rejects a turn that lost it.
            Some("thinking") => reasoning.push_str(block.thinking.as_deref().unwrap_or_default()),
            Some("tool_use") => {
                let position = calls.len();
                calls.push(ContentBlock::ToolUse {
                    // A call the client did not name still needs an id for the
                    // result that answers it to point at, so one is synthesized
                    // from the call's position. See the module note in the tests
                    // about where this departs from the original.
                    id: tool_call_id(block.id.as_deref(), position),
                    name: block.name.clone().unwrap_or_default(),
                    input: block.input.clone().unwrap_or_else(|| json!({})),
                });
            }
            other => warnings.push(Warning::new(
                format!("{path}.type"),
                format!(
                    "`{}` is not an assistant block and was dropped",
                    other.unwrap_or("(none)")
                ),
            )),
        }
    }

    // A single cache-free text part is re-spelled as a plain string on the wire,
    // and an empty string is no content at all — so one empty part disappears
    // where a list of two keeps every part, empty ones included.
    if texts.len() <= 1 && !has_breakpoint && texts.first().and_then(ContentBlock::as_text) == Some("") {
        texts.clear();
    }

    let mut content = Vec::with_capacity(1 + texts.len() + calls.len());
    if !reasoning.is_empty() {
        content.push(ContentBlock::reasoning(reasoning));
    }
    content.extend(texts);
    content.extend(calls);
    CanonicalMessage::new(Role::Assistant, content)
}

/// One user turn, as the one or two turns it stands for.
///
/// Anthropic packs tool output into the user turn that follows it; OpenAI keeps
/// it in a turn of its own, ordered before the text. So the return is a list.
fn user(
    index: usize,
    message: &Message,
    names: &HashMap<String, String>,
    warnings: &mut Vec<Warning>,
) -> Vec<CanonicalMessage> {
    let mut text = String::new();
    let mut parts: Vec<ContentBlock> = Vec::new();
    let mut results: Vec<ContentBlock> = Vec::new();

    for (block_index, block) in body_blocks(message).iter().enumerate() {
        let path = format!("messages[{index}].content[{block_index}]");
        match block.kind.as_deref() {
            Some("text") => {
                let part = text_of(block);
                text.push_str(&part);
                parts.push(ContentBlock::Text {
                    text: part,
                    cache_control: breakpoint(block),
                });
            }
            Some("image") => match image_url(block) {
                Some(url) => parts.push(ContentBlock::Image {
                    source: ImageSource::from_url(url),
                    cache_control: None,
                }),
                None => warnings.push(Warning::new(
                    format!("{path}.source"),
                    "an image without a payload or a URL was dropped",
                )),
            },
            Some("tool_result") => results.push(tool_result(block, names)),
            other => warnings.push(Warning::new(
                format!("{path}.type"),
                format!("`{}` is not a user block and was dropped", other.unwrap_or("(none)")),
            )),
        }
    }

    let mut out = Vec::with_capacity(results.len() + 1);
    // Ahead of the text, whatever order they arrived in: a tool result has to
    // sit directly after the turn that asked for it or the upstream rejects it.
    out.extend(
        results
            .into_iter()
            .map(|block| CanonicalMessage::new(Role::Tool, vec![block])),
    );

    if !parts.is_empty() {
        out.push(CanonicalMessage::new(Role::User, parts));
    } else if !text.is_empty() {
        // Only reachable for a string body, which stands for one text part.
        out.push(CanonicalMessage::new(Role::User, vec![ContentBlock::text(text)]));
    }
    out
}

fn tool_result(block: &Block, names: &HashMap<String, String>) -> ContentBlock {
    let tool_use_id = block.tool_use_id.clone().unwrap_or_default();
    // A resumed conversation may reference a call the client trimmed away. The
    // name is then genuinely unknown, and an empty one is what the upstream
    // rejects, so the field is left out rather than blanked.
    let name = names.get(&tool_use_id).cloned().filter(|name| !name.is_empty());
    ContentBlock::ToolResult {
        tool_use_id,
        name,
        content: tool_output(block.content.as_ref()),
        // The wire has no field for this; it is kept for the audit trail, where
        // "the tool failed" is exactly what an operator needs to see.
        is_error: block.is_error.unwrap_or(false),
    }
}

/// A tool result's text.
///
/// A list of blocks is joined by newlines, which is how a client that packs
/// several observations into one result expects them to read back.
fn tool_output(content: Option<&Value>) -> String {
    match content {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| part.get("text").and_then(Value::as_str).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
    }
}

/// The URL an image block stands for.
///
/// An inline payload becomes the data URL the wire carries inline images as;
/// anything else falls back to the block's own URL.
fn image_url(block: &Block) -> Option<String> {
    let source = block.source.as_ref()?;
    let data = source.data.as_deref().filter(|data| !data.is_empty());
    if source.kind.as_deref() == Some("base64")
        && let Some(data) = data
    {
        let media_type = source.media_type.as_deref().unwrap_or("image/png");
        return Some(format!("data:{media_type};base64,{data}"));
    }
    source.url.clone().filter(|url| !url.is_empty())
}

/// The message's blocks.
///
/// A body that is not a list stands for the one text block it spells, which is
/// how a client that sends `content: "hi"` means it.
fn body_blocks(message: &Message) -> Vec<Block> {
    match message.content.as_ref() {
        None => vec![Block::text(String::new())],
        Some(MessageContent::Text(text)) => vec![Block::text(text.clone())],
        Some(MessageContent::Blocks(blocks)) => blocks.clone(),
        Some(MessageContent::Other(other)) => vec![Block::text(value_text(other))],
    }
}

fn text_of(block: &Block) -> String {
    block.text.clone().unwrap_or_default()
}

fn breakpoint(block: &Block) -> Option<bifrost_core::CacheControl> {
    block.cache_control.as_ref().map(bifrost_core::CacheControl::from_json)
}

/// A JSON value as the text it stands for.
fn value_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(body: Value) -> Decoded<CanonicalRequest> {
        decode(&serde_json::to_vec(&body).expect("serialize")).expect("decode")
    }

    #[test]
    fn a_bare_string_message_is_one_text_turn() {
        let decoded = convert(json!({ "messages": [{ "role": "user", "content": "hi" }] }));
        assert_eq!(decoded.value.messages.len(), 1);
        assert_eq!(
            decoded.value.messages[0],
            CanonicalMessage::new(Role::User, vec![ContentBlock::text("hi")])
        );
    }

    #[test]
    fn a_single_empty_text_part_survives_on_a_user_turn_but_not_on_an_assistant_one() {
        let user = convert(json!({
            "messages": [{ "role": "user", "content": [{ "type": "text", "text": "" }] }]
        }));
        assert_eq!(
            user.value.messages[0],
            CanonicalMessage::new(Role::User, vec![ContentBlock::text("")]),
            "the user turn is not re-spelled, so its part is kept"
        );

        let assistant = convert(json!({
            "messages": [{ "role": "assistant", "content": [{ "type": "text", "text": "" }] }]
        }));
        assert!(
            assistant.value.messages[0].content.is_empty(),
            "one empty part collapses to no content at all"
        );
    }

    #[test]
    fn tool_results_precede_the_text_they_arrived_with() {
        let decoded = convert(json!({
            "messages": [
                { "role": "assistant", "content": [
                    { "type": "tool_use", "id": "toolu_1", "name": "search", "input": { "q": "x" } }
                ]},
                { "role": "user", "content": [
                    { "type": "text", "text": "and now?" },
                    { "type": "tool_result", "tool_use_id": "toolu_1", "content": "found 3" }
                ]}
            ]
        }));

        let roles: Vec<Role> = decoded.value.messages.iter().map(|message| message.role).collect();
        assert_eq!(roles, vec![Role::Assistant, Role::Tool, Role::User]);
        assert_eq!(
            decoded.value.messages[1].content[0],
            ContentBlock::ToolResult {
                tool_use_id: "toolu_1".to_owned(),
                name: Some("search".to_owned()),
                content: "found 3".to_owned(),
                is_error: false,
            }
        );
    }

    #[test]
    fn a_tool_result_without_a_matching_call_carries_no_name() {
        let decoded = convert(json!({
            "messages": [{ "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": "toolu_gone", "content": "orphan" }
            ]}]
        }));
        let ContentBlock::ToolResult { name, .. } = &decoded.value.messages[0].content[0] else {
            panic!("expected a tool result");
        };
        assert_eq!(*name, None);
    }

    #[test]
    fn reasoning_is_collected_and_leads_the_turn() {
        let decoded = convert(json!({
            "messages": [{ "role": "assistant", "content": [
                { "type": "text", "text": "answer" },
                { "type": "thinking", "thinking": "why " },
                { "type": "thinking", "thinking": "not" }
            ]}]
        }));
        assert_eq!(
            decoded.value.messages[0].content,
            vec![ContentBlock::reasoning("why not"), ContentBlock::text("answer"),]
        );
    }

    #[test]
    fn an_inline_image_keeps_its_payload_and_type_apart() {
        let decoded = convert(json!({
            "messages": [{ "role": "user", "content": [
                { "type": "image", "source": { "type": "base64", "data": "AAAA" } },
                { "type": "image", "source": { "type": "url", "url": "https://example.test/a.png" } }
            ]}]
        }));
        assert_eq!(
            decoded.value.messages[0].content,
            vec![
                ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".to_owned(),
                        data: "AAAA".to_owned(),
                    },
                    cache_control: None,
                },
                ContentBlock::Image {
                    source: ImageSource::Url {
                        url: "https://example.test/a.png".to_owned(),
                        media_type: None,
                    },
                    cache_control: None,
                },
            ],
            "an inline payload is split, because the wire carries its type separately"
        );
    }

    #[test]
    fn an_empty_prompt_is_no_prompt_even_with_a_breakpoint() {
        let decoded = convert(json!({
            "system": [{ "type": "text", "text": "", "cache_control": { "type": "ephemeral" } }],
            "messages": [{ "role": "user", "content": "hi" }]
        }));
        assert!(decoded.value.system.is_empty());
    }

    #[test]
    fn sections_keep_their_own_breakpoints() {
        let decoded = convert(json!({
            "system": [
                { "type": "text", "text": "stable", "cache_control": { "type": "ephemeral" } },
                { "type": "text", "text": "volatile" }
            ],
            "messages": []
        }));
        assert_eq!(decoded.value.system.len(), 2);
        assert!(decoded.value.system[0].is_cache_breakpoint());
        assert!(!decoded.value.system[1].is_cache_breakpoint());
    }

    #[test]
    fn a_breakpoint_kind_is_kept_rather_than_normalized() {
        let decoded = convert(json!({
            "system": [{ "type": "text", "text": "x", "cache_control": { "type": "1h" } }],
            "messages": []
        }));
        let Some(bifrost_core::CacheControl { kind }) = decoded.value.system[0].cache_control() else {
            panic!("expected a breakpoint");
        };
        assert_eq!(kind, "1h");
    }

    #[test]
    fn a_thinking_budget_selects_an_effort_level() {
        let level = |body: Value| convert(body).value.params.reasoning_effort;
        assert_eq!(
            level(json!({ "messages": [], "thinking": { "type": "enabled", "budget_tokens": 12000 } })),
            Some(ReasoningEffort::High)
        );
        assert_eq!(
            level(json!({ "messages": [], "thinking": { "type": "enabled", "budget_tokens": 6000 } })),
            Some(ReasoningEffort::Medium)
        );
        assert_eq!(
            level(json!({ "messages": [], "thinking": { "type": "enabled", "budget_tokens": 100 } })),
            Some(ReasoningEffort::Low)
        );
        assert_eq!(
            level(json!({ "messages": [], "thinking": { "type": "adaptive" } })),
            Some(ReasoningEffort::Medium),
            "an adaptive budget with no level asks for the middle one"
        );
        assert_eq!(
            level(json!({ "messages": [], "thinking": { "type": "adaptive", "effort": "max" } })),
            Some(ReasoningEffort::Max)
        );
        assert_eq!(
            level(json!({ "messages": [], "thinking": { "type": "disabled" } })),
            None,
            "sending nothing is how this endpoint says `do not think`"
        );
    }

    #[test]
    fn the_tool_choice_vocabulary_maps_onto_the_canonical_one() {
        let choice = |body: Value| convert(body).value.tool_choice;
        assert_eq!(
            choice(json!({ "messages": [], "tool_choice": {} })),
            Some(ToolChoice::Auto)
        );
        assert_eq!(
            choice(json!({ "messages": [], "tool_choice": { "type": "any" } })),
            Some(ToolChoice::Required)
        );
        assert_eq!(
            choice(json!({ "messages": [], "tool_choice": { "type": "none" } })),
            Some(ToolChoice::None)
        );
        assert_eq!(
            choice(json!({ "messages": [], "tool_choice": { "type": "tool", "name": "search" } })),
            Some(ToolChoice::named("search"))
        );
        assert_eq!(
            choice(json!({ "messages": [], "tool_choice": { "type": "sometimes" } })),
            None,
            "a choice this build cannot name leaves the decision to the model"
        );
    }

    #[test]
    fn attribution_and_sampling_ride_along() {
        let decoded = convert(json!({
            "model": "claude-sonnet-4-6",
            "metadata": { "user_id": "user_1" },
            "max_tokens": 4096,
            "temperature": 0.25,
            "top_p": 0.9,
            "stop_sequences": ["\n\n"],
            "stream": true,
            "messages": []
        }));
        assert_eq!(decoded.value.user.as_deref(), Some("user_1"));
        assert_eq!(decoded.value.params.max_tokens, Some(4096));
        assert_eq!(decoded.value.params.top_p, Some(0.9));
        assert_eq!(decoded.value.params.stop, vec!["\n\n"]);
        assert!(decoded.value.stream);
    }

    #[test]
    fn unknown_shapes_are_reported_instead_of_fatal() {
        let decoded = convert(json!({
            "system": 12,
            "messages": [
                { "role": "system", "content": "ignored" },
                { "role": "user", "content": [{ "type": "video", "url": "x" }] }
            ]
        }));
        let paths: Vec<&str> = decoded.warnings.iter().map(|warning| warning.path.as_str()).collect();
        assert!(paths.contains(&"system"), "{paths:?}");
        assert!(paths.contains(&"messages[0].role"), "{paths:?}");
        assert!(paths.contains(&"messages[1].content[0].type"), "{paths:?}");
    }

    #[test]
    fn a_malformed_body_is_a_bad_request() {
        let error = decode(b"not json").expect_err("should fail");
        assert!(error.to_string().starts_with("invalid request"));
    }
}
