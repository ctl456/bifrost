//! The OpenAI Responses request → canonical IR.
//!
//! Three behaviors here are load-bearing, and all three were read off the
//! original rather than assumed:
//!
//! - **An item with no `type` is a message.** `type` is optional on the message
//!   arm of the input union, so `{"role":"user","content":"hi"}` is valid and
//!   common. Reading it as anything else drops the turn while the request still
//!   succeeds, which is the worst available failure.
//! - **A reasoning item, a message and its calls are one assistant turn.** The
//!   Responses protocol flattens them into sibling items; the wire format wants
//!   them on one message, so they are accumulated and flushed together.
//! - **A turn that was nothing but reasoning is dropped.** Accumulated reasoning
//!   is only kept when the same turn also carries text or calls, which is what
//!   the original does — reasoning with nothing to reason about is not a turn.
//!
//! `instructions` becomes the system prompt; everything the endpoint merely
//! echoes back is stored in [`CanonicalRequest::echo`] instead.

use std::collections::HashMap;

use bifrost_core::{
    CanonicalMessage, CanonicalRequest, ContentBlock, InferenceParams, ReasoningEffort, Role, ToolChoice,
    ToolDefinition,
};
use serde_json::{Map, Value, json};

use crate::openai::responses::schema::{ResponsesInput, ResponsesItem, ResponsesRequest};
use crate::warning::{Decoded, Warning};

/// Convert a decoded Responses body into the canonical IR.
pub fn decode(body: &[u8]) -> Result<Decoded<CanonicalRequest>, bifrost_core::ConversionError> {
    let wire: ResponsesRequest = serde_json::from_slice(body)
        .map_err(|error| bifrost_core::ConversionError::bad_request(format!("invalid request body: {error}")))?;

    // Refused rather than ignored: this proxy keeps no conversation state, so
    // honouring the field is impossible and dropping it would answer a follow-up
    // question as if it were the first one.
    if wire.previous_response_id.as_deref().is_some_and(|id| !id.is_empty()) {
        return Err(bifrost_core::ConversionError::bad_request_at(
            "previous_response_id is not supported (this proxy is stateless); send the full input each turn",
            "previous_response_id",
        ));
    }

    let mut decoded = Decoded::lossless(CanonicalRequest::new(Vec::new()));
    let warnings = &mut decoded.warnings;

    let mut builder = Builder::default();
    builder.absorb_instructions(wire.instructions.as_ref());
    builder.absorb(wire.input.as_ref(), warnings);
    builder.flush();

    // The original's check, before the split into prompt and messages: a request
    // whose only content was `instructions` is answered, and one that carried no
    // input at all is refused.
    if builder.pushed == 0 {
        return Err(bifrost_core::ConversionError::bad_request_at(
            "input is required",
            "input",
        ));
    }

    let request = &mut decoded.value;
    request.model = wire.model.clone();
    request.stream = wire.stream == Some(true);
    request.parallel_tool_calls = wire.parallel_tool_calls;
    request.params = params(&wire);
    request.tools = tools(&wire, warnings);
    request.tool_choice = tool_choice(&wire, warnings);
    request.system = builder.system;
    request.messages = builder.messages;
    request.echo = Some(echo(&wire));
    Ok(decoded)
}

/// The assistant turn being accumulated across input items.
#[derive(Debug, Default)]
struct Pending {
    reasoning: Option<String>,
    text: Option<String>,
    calls: Vec<(String, String, Value)>,
}

#[derive(Debug, Default)]
struct Builder {
    system: Vec<ContentBlock>,
    messages: Vec<CanonicalMessage>,
    /// How many entries the original would have appended to its message list.
    ///
    /// Tracked separately from [`Self::messages`] because the original counts
    /// system and conversation turns in one array before the split, and an
    /// `instructions`-only request is exactly the case where the two disagree.
    pushed: usize,
    pending: Option<Pending>,
    /// `call_id → function name`, so a result can be labelled without the client
    /// repeating the name the call already carried.
    call_names: HashMap<String, String>,
}

impl Builder {
    /// The top-level `instructions` field, which becomes the first system section.
    ///
    /// It is counted only when it yields text, because the original pushes it
    /// only then; a system *item*, by contrast, is pushed even when empty. The
    /// two rules differ, which is why they are not folded together — but both
    /// count, and a request whose only content is `instructions` is one the
    /// original answers rather than refuses.
    fn absorb_instructions(&mut self, instructions: Option<&Value>) {
        let text = text_of(instructions);
        if !text.is_empty() {
            self.pushed += 1;
            self.system.push(ContentBlock::text(text));
        }
    }

    fn absorb(&mut self, input: Option<&ResponsesInput>, warnings: &mut Vec<Warning>) {
        let items = match input {
            Some(ResponsesInput::Text(text)) => {
                self.pushed += 1;
                self.messages.push(CanonicalMessage::new(
                    Role::User,
                    vec![ContentBlock::text(text.clone())],
                ));
                return;
            }
            Some(ResponsesInput::Items(items)) => items,
            None => return,
        };

        for (index, item) in items.iter().enumerate() {
            self.absorb_item(index, item, warnings);
        }
    }

    fn absorb_item(&mut self, index: usize, item: &ResponsesItem, warnings: &mut Vec<Warning>) {
        match item.kind() {
            Some("reasoning") => {
                let reasoning = reasoning_of(item);
                if !reasoning.is_empty() {
                    self.pending_mut().reasoning = Some(reasoning);
                }
            }
            Some("message") => self.absorb_message(item),
            Some("function_call") => {
                let id = item
                    .call_id
                    .clone()
                    .or_else(|| item.id.clone())
                    .filter(|id| !id.is_empty())
                    .unwrap_or_else(|| format!("call_{index}"));
                let name = item.name.clone().unwrap_or_default();
                let arguments = arguments_of(item.arguments.as_ref());
                self.call_names.insert(id.clone(), name.clone());
                self.pending_mut().calls.push((id, name, arguments));
            }
            Some("function_call_output") => {
                self.flush();
                self.pushed += 1;
                let tool_use_id = item.call_id.clone().unwrap_or_default();
                let content = output_of(item.output.as_ref());
                self.messages.push(CanonicalMessage::new(
                    Role::Tool,
                    vec![ContentBlock::ToolResult {
                        name: self.call_names.get(&tool_use_id).cloned(),
                        tool_use_id,
                        content,
                        is_error: false,
                    }],
                ));
            }
            other => warnings.push(Warning::new(
                format!("input[{index}].type"),
                format!("unknown input item `{}` was dropped", other.unwrap_or("")),
            )),
        }
    }

    fn absorb_message(&mut self, item: &ResponsesItem) {
        let text = text_of(item.content.as_ref());
        match item.role.as_deref() {
            // An assistant turn joins whatever reasoning and calls surround it,
            // so it is accumulated rather than pushed.
            Some("assistant") => {
                if !text.is_empty() {
                    self.pending_mut().text = Some(text);
                }
            }
            Some("system" | "developer") => {
                self.pushed += 1;
                // An empty section is dropped here exactly as the wire encoder
                // would drop it, so the prompt is the same either way.
                if !text.is_empty() {
                    self.system.push(ContentBlock::text(text));
                }
            }
            _ => {
                self.flush();
                self.pushed += 1;
                self.messages
                    .push(CanonicalMessage::new(Role::User, vec![ContentBlock::text(text)]));
            }
        }
    }

    fn pending_mut(&mut self) -> &mut Pending {
        self.pending.get_or_insert_with(Pending::default)
    }

    /// Emit the accumulated assistant turn, if it has anything to say.
    fn flush(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if pending.text.is_none() && pending.calls.is_empty() {
            return;
        }

        let mut content = Vec::new();
        if let Some(reasoning) = pending.reasoning {
            content.push(ContentBlock::reasoning(reasoning));
        }
        if let Some(text) = pending.text {
            content.push(ContentBlock::text(text));
        }
        for (id, name, input) in pending.calls {
            content.push(ContentBlock::ToolUse { id, name, input });
        }
        self.pushed += 1;
        self.messages.push(CanonicalMessage::new(Role::Assistant, content));
    }
}

fn params(wire: &ResponsesRequest) -> InferenceParams {
    InferenceParams {
        max_tokens: wire.max_output_tokens,
        temperature: wire.temperature.filter(|value| value.is_finite()),
        top_p: wire.top_p.filter(|value| value.is_finite()),
        reasoning_effort: wire
            .reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.get("effort"))
            .and_then(Value::as_str)
            .filter(|effort| !effort.is_empty())
            .map(ReasoningEffort::parse),
        thinking_budget_tokens: None,
        stop: Vec::new(),
    }
}

/// Function definitions, in this endpoint's flat shape.
fn tools(wire: &ResponsesRequest, warnings: &mut Vec<Warning>) -> Vec<ToolDefinition> {
    let Some(tools) = &wire.tools else {
        return Vec::new();
    };
    tools
        .iter()
        .enumerate()
        .filter_map(|(index, tool)| {
            let Some(object) = tool.as_object() else {
                warnings.push(Warning::new(
                    format!("tools[{index}]"),
                    "a tool that is not an object was dropped",
                ));
                return None;
            };
            let name = object.get("name").and_then(Value::as_str).unwrap_or("");
            // The original keeps anything that is a function or that at least
            // named itself, and drops the rest.
            if object.get("type").and_then(Value::as_str) != Some("function") && name.is_empty() {
                warnings.push(Warning::new(
                    format!("tools[{index}]"),
                    "a tool that is neither a function nor named was dropped",
                ));
                return None;
            }
            Some(ToolDefinition {
                name: name.to_owned(),
                description: object
                    .get("description")
                    .and_then(Value::as_str)
                    .filter(|description| !description.is_empty())
                    .map(str::to_owned),
                parameters: object.get("parameters").filter(|value| !value.is_null()).cloned(),
            })
        })
        .collect()
}

/// `tool_choice` as a mode name or as an object naming one function.
fn tool_choice(wire: &ResponsesRequest, warnings: &mut Vec<Warning>) -> Option<ToolChoice> {
    match wire.tool_choice.as_ref()? {
        Value::String(mode) => Some(match mode.as_str() {
            "auto" => ToolChoice::Auto,
            "none" => ToolChoice::None,
            "required" => ToolChoice::Required,
            other => {
                warnings.push(Warning::new(
                    "tool_choice",
                    format!("unknown tool_choice `{other}`, treated as `auto`"),
                ));
                ToolChoice::Auto
            }
        }),
        Value::Object(object) => object
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(ToolChoice::named),
        // Anything else is no choice at all, which is what the original leaves
        // behind rather than inventing one.
        _ => None,
    }
}

/// What the endpoint answers with when it echoes the request back.
///
/// Built here rather than in the renderer so the client's payload is read once,
/// at the boundary, and the response path only has to copy what it finds.
fn echo(wire: &ResponsesRequest) -> Value {
    let mut echo = Map::new();
    echo.insert(
        "instructions".to_owned(),
        wire.instructions.clone().unwrap_or(Value::Null),
    );
    echo.insert(
        "max_output_tokens".to_owned(),
        wire.max_output_tokens.map_or(Value::Null, |tokens| json!(tokens)),
    );
    // Absent, not null: the body distinguishes "the client said nothing", which
    // is reported as the protocol's default, from a value the client chose.
    if let Some(temperature) = wire.temperature {
        echo.insert("temperature".to_owned(), json!(temperature));
    }
    if let Some(top_p) = wire.top_p {
        echo.insert("top_p".to_owned(), json!(top_p));
    }
    echo.insert(
        "reasoning".to_owned(),
        wire.reasoning
            .clone()
            .filter(|value| !value.is_null())
            .unwrap_or(Value::Null),
    );
    // Only a mode name survives: the endpoint reports what it was told to prefer,
    // and an object-shaped choice is reported as the default.
    echo.insert(
        "tool_choice".to_owned(),
        match wire.tool_choice.as_ref() {
            Some(Value::String(mode)) => json!(mode),
            _ => json!("auto"),
        },
    );
    echo.insert("tools".to_owned(), Value::Array(wire.tools.clone().unwrap_or_default()));
    Value::Object(echo)
}

/// The text of a string-or-parts field.
fn text_of(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| part.get("text").and_then(Value::as_str).unwrap_or_default())
            .collect(),
        _ => String::new(),
    }
}

/// A reasoning item's thinking, however it was spelled.
fn reasoning_of(item: &ResponsesItem) -> String {
    for field in [item.summary.as_ref(), item.content.as_ref()] {
        // Only a non-empty list counts: an empty one falls through to the next
        // spelling, exactly as the original's truthiness check does.
        if let Some(Value::Array(parts)) = field {
            if !parts.is_empty() {
                return parts
                    .iter()
                    .map(|part| part.get("text").and_then(Value::as_str).unwrap_or_default())
                    .collect();
            }
        }
    }
    item.text.clone().unwrap_or_default()
}

/// A call's arguments, parsed the way the wire format wants them.
fn arguments_of(raw: Option<&Value>) -> Value {
    match raw {
        // An encoded string is what a client replays from an earlier response,
        // so it is parsed rather than forwarded as a string.
        Some(Value::String(text)) => serde_json::from_str(text).unwrap_or_else(|_| json!({})),
        None | Some(Value::Null) => json!({}),
        Some(other) => other.clone(),
    }
}

/// A call result, stringified when it is not already a string.
fn output_of(raw: Option<&Value>) -> String {
    match raw {
        Some(Value::String(text)) => text.clone(),
        None | Some(Value::Null) => String::new(),
        Some(other) => other.to_string(),
    }
}
