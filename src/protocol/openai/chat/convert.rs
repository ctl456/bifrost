//! Chat Completions → canonical IR.
//!
//! The mapping is a faithful port of the original proxy, because every branch
//! here was learned from a real request the upstream rejected. Two are worth
//! calling out before reading:
//!
//! - **System blocks are newline-joined, except the last.** The upstream client
//!   composes prompt sections that way, and the trailing separator changes the
//!   prefix the provider caches against.
//! - **An assistant turn is ordered reasoning → text → tool-call.** The upstream
//!   rejects a history whose reasoning was dropped, and it compares the order,
//!   not just the content.
//! - **A user part the build does not model is forwarded, not dropped.** See
//!   [`ContentBlock::Opaque`]: only the upstream knows what an unfamiliar part
//!   means, so it is the upstream that gets to reject it.

use crate::core::{
    CanonicalMessage, CanonicalRequest, ContentBlock, ImageSource, InferenceParams, ReasoningEffort, Role, ToolChoice,
    ToolDefinition,
};

use crate::protocol::openai::chat::schema::{
    ArgumentPayload, ChatCompletionRequest, ChatMessage, ContentPart, MessageContent, StopSequence,
};
use crate::protocol::openai::shared::OpenAiToolChoice;
use crate::protocol::warning::Decoded;

/// Convert a decoded Chat Completions body into the canonical IR.
///
/// `body` is the raw request, so a malformed payload is reported with the JSON
/// error's own position rather than as a missing field.
pub fn decode(body: &[u8]) -> Result<Decoded<CanonicalRequest>, crate::core::ConversionError> {
    let wire: ChatCompletionRequest = serde_json::from_slice(body)
        .map_err(|error| crate::core::ConversionError::bad_request(format!("invalid request body: {error}")))?;

    let mut decoded = Decoded::lossless(CanonicalRequest::new(Vec::new()));
    let request = &mut decoded.value;

    request.model = wire.model.clone();
    request.stream = wire.stream == Some(true);
    request.parallel_tool_calls = wire.parallel_tool_calls;
    request.prompt_cache_key = wire.prompt_cache_key.clone();
    request.user = wire.user.clone().filter(|user| !user.is_empty());
    request.tools = decode_tools(&wire, &mut decoded.warnings);
    request.tool_choice = decode_tool_choice(&wire, &mut decoded.warnings);
    request.params = decode_params(&wire);

    let system = decode_system(&wire.messages);
    request.messages = decode_messages(&wire.messages, &mut decoded.warnings);
    request.system = system;

    Ok(decoded)
}

fn decode_params(wire: &ChatCompletionRequest) -> InferenceParams {
    InferenceParams {
        // `max_completion_tokens` is the newer spelling of the same field; it wins
        // only when `max_tokens` is absent so an older client that sends both
        // keeps the meaning it wrote first.
        max_tokens: wire.max_tokens.or(wire.max_completion_tokens),
        temperature: wire.temperature.filter(|value| value.is_finite()),
        top_p: wire.top_p.filter(|value| value.is_finite()),
        reasoning_effort: wire.reasoning_effort.as_deref().map(ReasoningEffort::parse),
        thinking_budget_tokens: None,
        stop: wire.stop.clone().map_or_else(Vec::new, StopSequence::into_vec),
    }
}

/// Tool definitions, aliasing applied later by the wire encoder.
fn decode_tools(
    wire: &ChatCompletionRequest,
    warnings: &mut Vec<crate::protocol::warning::Warning>,
) -> Vec<ToolDefinition> {
    let Some(tools) = &wire.tools else {
        return Vec::new();
    };
    tools
        .iter()
        .enumerate()
        .filter_map(|(index, tool)| {
            if tool.r#type != "function" {
                warnings.push(crate::protocol::warning::Warning::new(
                    format!("tools[{index}].type"),
                    format!("`{}` tools are not supported and were dropped", tool.r#type),
                ));
                return None;
            }
            Some(ToolDefinition {
                name: tool.function.name.clone(),
                description: tool.function.description.clone(),
                parameters: tool.function.parameters.clone(),
            })
        })
        .collect()
}

fn decode_tool_choice(
    wire: &ChatCompletionRequest,
    warnings: &mut Vec<crate::protocol::warning::Warning>,
) -> Option<ToolChoice> {
    match wire.tool_choice.as_ref()? {
        OpenAiToolChoice::Mode(mode) => Some(match mode.as_str() {
            "auto" => ToolChoice::Auto,
            "none" => ToolChoice::None,
            "required" => ToolChoice::Required,
            other => {
                warnings.push(crate::protocol::warning::Warning::new(
                    "tool_choice",
                    format!("unknown tool_choice `{other}`, treated as `auto`"),
                ));
                ToolChoice::Auto
            }
        }),
        OpenAiToolChoice::Function { r#type, function } => match r#type.as_str() {
            "function" => match function.as_ref().map(|named| named.name.clone()) {
                Some(name) if !name.is_empty() => Some(ToolChoice::named(name)),
                _ => {
                    warnings.push(crate::protocol::warning::Warning::new(
                        "tool_choice.function.name",
                        "a function tool choice without a name was treated as `auto`",
                    ));
                    Some(ToolChoice::Auto)
                }
            },
            // An object-shaped choice, as Anthropic-style clients sometimes send.
            "auto" => Some(ToolChoice::Auto),
            "any" => Some(ToolChoice::Required),
            "none" => Some(ToolChoice::None),
            "tool" => function
                .as_ref()
                .map(|named| ToolChoice::named(named.name.clone()))
                .or_else(|| {
                    warnings.push(crate::protocol::warning::Warning::new(
                        "tool_choice.name",
                        "a named tool choice without a name was treated as `auto`",
                    ));
                    Some(ToolChoice::Auto)
                }),
            other => {
                warnings.push(crate::protocol::warning::Warning::new(
                    "tool_choice.type",
                    format!("unknown tool_choice type `{other}`, treated as `auto`"),
                ));
                Some(ToolChoice::Auto)
            }
        },
    }
}

/// System and developer turns become the prompt's system sections.
fn decode_system(messages: &[ChatMessage]) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();
    for message in messages {
        if !is_system_role(&message.role) {
            continue;
        }
        match &message.content {
            None => {}
            Some(MessageContent::Text(text)) => {
                if !text.is_empty() {
                    blocks.push(ContentBlock::text(text.clone()));
                }
            }
            Some(MessageContent::Parts(parts)) => {
                for part in parts {
                    let cache_control = cache_breakpoint(part);
                    let text = text_of_part(part);
                    // An empty section is only worth keeping when it carries a
                    // cache breakpoint — that is the client asking for a cache
                    // boundary, not for content.
                    if text.is_empty() && cache_control.is_none() {
                        continue;
                    }
                    blocks.push(ContentBlock::Text { text, cache_control });
                }
            }
        }
    }

    // Sections are *not* newline-joined here. The separator is a property of the
    // wire dialect, not of the prompt, so the `cc` encoder owns it — doing it in
    // both places would double every separator.
    blocks
}

fn is_system_role(role: &str) -> bool {
    role == "system" || role == "developer"
}

/// The text a client addressed to a text-shaped part.
///
/// `text` is the documented field; the original also accepted `content`, which
/// some wrappers emit.
fn text_of_part(part: &ContentPart) -> String {
    part.text.clone().or_else(|| part.content.clone()).unwrap_or_default()
}

fn decode_messages(
    messages: &[ChatMessage],
    warnings: &mut Vec<crate::protocol::warning::Warning>,
) -> Vec<CanonicalMessage> {
    let tool_names = tool_names(messages);
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| !is_system_role(&message.role))
        .map(|(index, message)| decode_message(index, message, &tool_names, warnings))
        .collect()
}

/// `tool_call_id → tool name`, from the assistant turns that made the calls.
fn tool_names(messages: &[ChatMessage]) -> std::collections::HashMap<String, String> {
    let mut names = std::collections::HashMap::new();
    for message in messages {
        let Some(calls) = &message.tool_calls else {
            continue;
        };
        for call in calls {
            if let Some(id) = call.id.as_deref().filter(|id| !id.is_empty()) {
                names.insert(id.to_owned(), call.function.name.clone());
            }
        }
    }
    names
}

fn decode_message(
    index: usize,
    message: &ChatMessage,
    tool_names: &std::collections::HashMap<String, String>,
    warnings: &mut Vec<crate::protocol::warning::Warning>,
) -> CanonicalMessage {
    match message.role.as_str() {
        "user" => CanonicalMessage::new(Role::User, user_content(index, message, warnings)),
        "assistant" => CanonicalMessage::new(Role::Assistant, assistant_content(index, message, warnings)),
        "tool" => CanonicalMessage::new(Role::Tool, tool_content(message, tool_names, warnings)),
        other => {
            warnings.push(crate::protocol::warning::Warning::new(
                format!("messages[{index}].role"),
                format!("unknown role `{other}`, treated as a user turn"),
            ));
            let text = message
                .content
                .as_ref()
                .map_or_else(String::new, |content| match content {
                    MessageContent::Text(text) => text.clone(),
                    MessageContent::Parts(parts) => parts.iter().map(text_of_part).collect(),
                });
            CanonicalMessage::new(Role::User, vec![ContentBlock::text(text)])
        }
    }
}

fn user_content(
    index: usize,
    message: &ChatMessage,
    warnings: &mut Vec<crate::protocol::warning::Warning>,
) -> Vec<ContentBlock> {
    match message.content.as_ref() {
        None => vec![ContentBlock::text(String::new())],
        Some(MessageContent::Text(text)) => vec![ContentBlock::text(text.clone())],
        Some(MessageContent::Parts(parts)) => {
            let mut blocks = Vec::with_capacity(parts.len());
            for (part_index, part) in parts.iter().enumerate() {
                let path = format!("messages[{index}].content[{part_index}]");
                match part.kind.as_str() {
                    "image_url" => match part.image_url.as_ref() {
                        Some(image) => blocks.push(ContentBlock::Image {
                            source: ImageSource::from_url(image.url.as_str()),
                            cache_control: cache_breakpoint(part),
                        }),
                        None => warnings.push(crate::protocol::warning::Warning::new(
                            format!("{path}.image_url"),
                            "an image part without a URL was dropped",
                        )),
                    },
                    "text" => {
                        let text = text_of_part(part);
                        let cache_control = cache_breakpoint(part);
                        if text.is_empty() && cache_control.is_none() {
                            continue;
                        }
                        blocks.push(ContentBlock::Text { text, cache_control });
                    }
                    other => {
                        warnings.push(crate::protocol::warning::Warning::new(
                            format!("{path}.type"),
                            format!("content part `{other}` is not modelled and was forwarded verbatim"),
                        ));
                        blocks.push(ContentBlock::opaque(part.verbatim()));
                    }
                }
            }
            blocks
        }
    }
}

fn assistant_content(
    index: usize,
    message: &ChatMessage,
    warnings: &mut Vec<crate::protocol::warning::Warning>,
) -> Vec<ContentBlock> {
    let mut blocks = Vec::with_capacity(3);

    // Reasoning first: the upstream compares the order, not just the presence.
    if let Some(reasoning) = message.reasoning_content.as_ref().filter(|text| !text.is_empty()) {
        blocks.push(ContentBlock::reasoning(reasoning.clone()));
    }

    match message.content.as_ref() {
        None => {}
        Some(MessageContent::Text(text)) => {
            if !text.is_empty() {
                blocks.push(ContentBlock::text(text.clone()));
            }
        }
        Some(MessageContent::Parts(parts)) => {
            for (part_index, part) in parts.iter().enumerate() {
                match part.kind.as_str() {
                    "text" => blocks.push(ContentBlock::Text {
                        text: text_of_part(part),
                        cache_control: cache_breakpoint(part),
                    }),
                    // Some clients put thinking in the content array instead of
                    // the dedicated field; only one of the two is kept so the
                    // reasoning is not duplicated.
                    "reasoning" | "thinking" if message.reasoning_content.is_none() => {
                        blocks.push(ContentBlock::reasoning(text_of_part(part)));
                    }
                    other => warnings.push(crate::protocol::warning::Warning::new(
                        format!("messages[{index}].content[{part_index}].type"),
                        format!("`{other}` is not a valid assistant content part and was dropped"),
                    )),
                }
            }
        }
    }

    if let Some(calls) = &message.tool_calls {
        for (call_index, call) in calls.iter().enumerate() {
            let path = format!("messages[{index}].tool_calls[{call_index}]");
            if call.r#type.as_deref().is_some_and(|kind| kind != "function") {
                warnings.push(crate::protocol::warning::Warning::new(
                    format!("{path}.type"),
                    "only function tool calls are supported; the rest were dropped",
                ));
            }
            let id = call
                .id
                .clone()
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| crate::core::tool_call_id(None, call_index));
            blocks.push(ContentBlock::ToolUse {
                id,
                name: call.function.name.clone(),
                input: argument_value(&call.function.arguments, &path, warnings),
            });
        }
    }

    blocks
}

fn tool_content(
    message: &ChatMessage,
    tool_names: &std::collections::HashMap<String, String>,
    warnings: &mut Vec<crate::protocol::warning::Warning>,
) -> Vec<ContentBlock> {
    let tool_use_id = message.tool_call_id.clone().unwrap_or_default();
    // A resumed conversation may reference a call the client trimmed away. The
    // name is then genuinely unknown, and sending an empty one is what the
    // upstream rejects — so it is left out rather than blanked.
    let name = tool_names
        .get(&tool_use_id)
        .cloned()
        .or_else(|| message.name.clone().filter(|name| !name.is_empty()));
    if name.is_none() {
        warnings.push(crate::protocol::warning::Warning::new(
            "tool_call_id",
            format!("no tool call found for `{tool_use_id}`; the result carries no name"),
        ));
    }
    vec![ContentBlock::ToolResult {
        tool_use_id,
        name,
        content: plain_text(message.content.as_ref()),
        is_error: false,
    }]
}

/// Flatten a tool message's content the way the upstream client does: text
/// blocks only, joined by newlines.
fn plain_text(content: Option<&MessageContent>) -> String {
    match content {
        None => String::new(),
        Some(MessageContent::Text(text)) => text.clone(),
        Some(MessageContent::Parts(parts)) => parts
            .iter()
            .filter(|part| part.kind == "text")
            .map(text_of_part)
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn cache_breakpoint(part: &ContentPart) -> Option<crate::core::CacheControl> {
    part.cache_control.as_ref().map(crate::core::CacheControl::from_json)
}

/// The parsed argument value for a tool call.
///
/// An unparseable argument string becomes `{}` rather than failing the request,
/// matching the original proxy: a call with no arguments is recoverable, a
/// rejected turn is not.
fn argument_value(
    payload: &ArgumentPayload,
    path: &str,
    warnings: &mut Vec<crate::protocol::warning::Warning>,
) -> serde_json::Value {
    match payload {
        ArgumentPayload::Absent => serde_json::json!({}),
        ArgumentPayload::Structured(value) => value.clone(),
        ArgumentPayload::Text(raw) => match serde_json::from_str(raw) {
            Ok(value) => value,
            Err(_) => {
                warnings.push(crate::protocol::warning::Warning::new(
                    format!("{path}.function.arguments"),
                    "arguments were not valid JSON and were treated as empty",
                ));
                serde_json::json!({})
            }
        },
    }
}
