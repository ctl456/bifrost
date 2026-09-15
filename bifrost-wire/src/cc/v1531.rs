//! The `cc/1.53.1` dialect.
//!
//! Aligned with the official client's wire format. Two details are load-bearing
//! and easy to "clean up" by accident:
//!
//! - **Key order matters.** The envelope is emitted in the client's own order,
//!   and several fields are positional rather than sorted.
//! - **`tools` is always present**, even as an empty array. An absent key and an
//!   empty array are observably different on the wire.

use std::collections::HashMap;

use bifrost_core::{
    CanonicalMessage, CanonicalRequest, ContentBlock, ConversionError, ImageSource, Role, ToolChoice, ToolDefinition,
};
use serde_json::{Map, Value, json};

use crate::adapter::WireAdapter;
use crate::context::WireContext;
use crate::entropy::Entropy;
use crate::request::WireRequest;

/// Where generation requests are posted.
pub const GENERATE_PATH: &str = "/alpha/generate";
/// Where a key's device identity is recorded.
pub const FINGERPRINT_PATH: &str = "/alpha/fingerprint/record";
/// Where a key's lifecycle events are posted.
pub const LIFECYCLE_PATH: &str = "/alpha/lifecycle-events";
/// Where the upstream publishes the models an account may use.
///
/// Under `/provider`, not `/alpha`: it is the service's own catalogue rather than
/// the client's private API, and it is the one path here that answers a `GET`.
pub const MODELS_PATH: &str = "/provider/v1/models";
/// The lifecycle event the client reports once a session exists.
///
/// The vocabulary belongs to the upstream, so it is spelled here rather than
/// invented from the event's meaning.
const LIFECYCLE_EVENT: &str = "cli_session_exists";
/// The client mode reported in a lifecycle event.
///
/// The original reads `CC_CLI_SESSION_MODE`, an enum of its own: `interactive`
/// or `non-interactive`. This build reports the default, because nothing else in
/// the envelope is configurable and a second vocabulary of modes would be one
/// more thing to keep consistent.
const LIFECYCLE_MODE: &str = "interactive";
/// Hex characters in a lifecycle session id, which is `sess_` plus eight bytes.
const LIFECYCLE_SESSION_BYTES: usize = 8;
const USER_AGENT: &str = "cli";
const CLI_ENVIRONMENT: &str = "production";
const TASTE_LEARNING: &str = "false";
const SERVICE_NAME: &str = "command-code";
const PROTOCOL_VERSION: &str = "1.53.1";
/// Where the client this dialect was read from is published.
///
/// The version reported upstream is the one above, always. This names the source
/// that version was read out of, so a deployment can be told when the source has
/// moved on without the dialect moving with it.
const RELEASE_PACKAGE: &str = "command-code";

/// Tool names the client rewrites before sending.
///
/// Applied to tool *definitions* only. History keeps whatever the model actually
/// emitted, which is already the rewritten name.
const TOOL_NAME_ALIASES: &[(&str, &str)] = &[
    ("bash_output", "shell_output"),
    ("task_output", "shell_output"),
    ("tool_search", "search_tools"),
    ("read_multiple_files", "read_file"),
];

/// Settings that are not per-request.
#[derive(Debug, Clone)]
pub struct CcV1531 {
    /// Used when the client names no model.
    pub default_model: String,
    /// Upper bound applied after the client's own `max_tokens`.
    pub max_tokens_cap: u32,
    /// Send a placeholder system block when the client sends none.
    ///
    /// Without it the upstream injects roughly 7.5K tokens of its own default
    /// prompt into the prefix, which both costs tokens and makes the model think
    /// it is running inside the vendor's own directory.
    pub empty_system_placeholder: bool,
}

impl Default for CcV1531 {
    fn default() -> Self {
        Self {
            default_model: "deepseek/deepseek-v4-flash".to_owned(),
            max_tokens_cap: 200_000,
            empty_system_placeholder: true,
        }
    }
}

impl WireAdapter for CcV1531 {
    fn name(&self) -> &'static str {
        SERVICE_NAME
    }

    fn version(&self) -> &'static str {
        PROTOCOL_VERSION
    }

    fn published_package(&self) -> Option<&'static str> {
        Some(RELEASE_PACKAGE)
    }

    fn encode(&self, request: &CanonicalRequest, context: &WireContext) -> Result<WireRequest, ConversionError> {
        let params = self.params(request, context);
        let body = envelope(&params, context);

        Ok(WireRequest {
            method: "POST",
            path: GENERATE_PATH.to_owned(),
            headers: headers(context),
            body: Some(body),
        })
    }

    fn announce(
        &self,
        context: &WireContext,
        fingerprint: Option<&bifrost_fingerprint::Fingerprint>,
        entropy: &dyn Entropy,
    ) -> Vec<WireRequest> {
        // A narrower header set than a generate carries: these endpoints are told
        // who is calling and what version is calling, and nothing about a
        // directory, a session or a trace.
        let mut announcement = identity_headers(&context.api_key);
        if context.zdr {
            announcement.push(("x-cmd-zdr".to_owned(), "1".to_owned()));
        }

        let mut requests = Vec::new();
        if let Some(fingerprint) = fingerprint {
            requests.push(WireRequest {
                method: "POST",
                path: FINGERPRINT_PATH.to_owned(),
                headers: announcement.clone(),
                // The record is the device itself, in the shape the client's own
                // collector produces — the upstream stores it against the key.
                body: Some(serde_json::to_value(fingerprint).unwrap_or(Value::Null)),
            });
        }

        // The event's `os` is the same platform and architecture the envelope
        // reports, so a deployment cannot describe itself two ways. It comes from
        // the device profile rather than from the fingerprint, because the event
        // is sent whether or not the device is.
        requests.push(WireRequest {
            method: "POST",
            path: LIFECYCLE_PATH.to_owned(),
            headers: announcement,
            body: Some(json!({
                "eventType": LIFECYCLE_EVENT,
                "metadata": {
                    "sessionId": format!("sess_{}", hex(entropy, LIFECYCLE_SESSION_BYTES)),
                    "cliVersion": PROTOCOL_VERSION,
                    "mode": LIFECYCLE_MODE,
                    "os": format!("{}-{}", context.device.platform, context.device.arch),
                },
            })),
        });
        requests
    }

    fn provider_models(&self, api_key: &str) -> Option<WireRequest> {
        // The headers of an announcement, and none of a generate's: this is about
        // an account, so it has no session, no directory and no trace.
        Some(WireRequest {
            method: "GET",
            path: MODELS_PATH.to_owned(),
            headers: identity_headers(api_key),
            body: None,
        })
    }
}

/// What every request says about who is calling, shared so the two narrower sets
/// cannot drift apart the way they did when `User-Agent` was added to one of them.
///
/// `User-Agent` is load-bearing rather than decorative. The upstream's edge
/// answers a request that carries none with `403 error code: 1010`, before it
/// looks at the path at all; every other header here was dropped one at a time
/// against the live service and the request still succeeded. Any value passes —
/// what matters is that the header exists — and `cli` is the value the client
/// sends.
fn identity_headers(api_key: &str) -> Vec<(String, String)> {
    vec![
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("User-Agent".to_owned(), USER_AGENT.to_owned()),
        ("x-command-code-version".to_owned(), PROTOCOL_VERSION.to_owned()),
        ("x-cli-environment".to_owned(), CLI_ENVIRONMENT.to_owned()),
        ("Authorization".to_owned(), format!("Bearer {api_key}")),
    ]
}

/// Random hex, for a handle that must not repeat between two announcements.
fn hex(entropy: &dyn Entropy, bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    entropy.fill(&mut buffer);
    let mut out = String::with_capacity(bytes * 2);
    for byte in &buffer {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

impl CcV1531 {
    fn params(&self, request: &CanonicalRequest, context: &WireContext) -> Value {
        let mut params = Map::new();
        params.insert(
            "model".to_owned(),
            json!(request.model.clone().unwrap_or_else(|| self.default_model.clone())),
        );
        params.insert("messages".to_owned(), Value::Array(cc_messages(&request.messages)));
        params.insert(
            "max_tokens".to_owned(),
            json!(request.params.effective_max_tokens().min(self.max_tokens_cap)),
        );
        // The upstream endpoint is streaming-only, whatever the client asked for.
        params.insert("stream".to_owned(), json!(true));

        if let Some(system) = self.system(request) {
            params.insert("system".to_owned(), system);
        }
        if let Some(temperature) = request.params.temperature {
            params.insert("temperature".to_owned(), json!(temperature));
        }
        if let Some(effort) = &request.params.reasoning_effort {
            params.insert("reasoning_effort".to_owned(), json!(effort.as_str()));
        }
        // Always present, even when empty.
        params.insert("tools".to_owned(), Value::Array(cc_tools(&request.tools)));
        if let Some(choice) = &request.tool_choice {
            params.insert("tool_choice".to_owned(), cc_tool_choice(choice));
        }
        if let Some(parallel) = request.parallel_tool_calls {
            params.insert("parallel_tool_calls".to_owned(), json!(parallel));
        }

        let _ = context;
        Value::Object(params)
    }

    /// System blocks, newline-joined the way the client composes prompt sections.
    fn system(&self, request: &CanonicalRequest) -> Option<Value> {
        let blocks: Vec<&ContentBlock> = request
            .system
            .iter()
            .filter(|block| matches!(block, ContentBlock::Text { .. }))
            .collect();

        if blocks.is_empty() {
            return self
                .empty_system_placeholder
                .then(|| json!([{ "type": "text", "text": " " }]));
        }

        let last = blocks.len() - 1;
        let mut out = Vec::with_capacity(blocks.len());
        for (index, block) in blocks.into_iter().enumerate() {
            let ContentBlock::Text { text, cache_control } = block else {
                continue;
            };
            let mut entry = Map::new();
            entry.insert("type".to_owned(), json!("text"));
            // Every section but the last is terminated; the client joins them this way.
            if index == last {
                entry.insert("text".to_owned(), json!(text));
            } else {
                entry.insert("text".to_owned(), json!(format!("{text}\n")));
            }
            if let Some(cache) = cache_control {
                entry.insert("cache_control".to_owned(), json!({ "type": cache.kind }));
            }
            out.push(Value::Object(entry));
        }
        Some(Value::Array(out))
    }
}

/// The envelope, in the order the client emits it.
fn envelope(params: &Value, context: &WireContext) -> Value {
    let mut body = Map::new();
    body.insert(
        "config".to_owned(),
        json!({
            "workingDir": context.device.project_dir,
            "date": context.today,
            "environment": context.device.platform,
            "structure": [],
            "isGitRepo": false,
            "currentBranch": "",
            "mainBranch": "",
            "gitStatus": "",
            "recentCommits": [],
        }),
    );
    body.insert("memory".to_owned(), Value::Null);
    body.insert("taste".to_owned(), Value::Null);
    // The client sends null here, not an empty string.
    body.insert("skills".to_owned(), Value::Null);
    body.insert("permissionMode".to_owned(), json!(context.permission_mode));
    // Omitted entirely when it is not a UUID.
    if let Some(thread_id) = context.thread_id() {
        body.insert("threadId".to_owned(), json!(thread_id));
    }
    body.insert("mode".to_owned(), json!(context.mode));
    body.insert("params".to_owned(), params.clone());
    Value::Object(body)
}

fn headers(context: &WireContext) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("User-Agent".to_owned(), USER_AGENT.to_owned()),
        ("x-command-code-version".to_owned(), PROTOCOL_VERSION.to_owned()),
        ("x-cli-environment".to_owned(), CLI_ENVIRONMENT.to_owned()),
        ("x-project-slug".to_owned(), context.project_slug.clone()),
        ("x-taste-learning".to_owned(), TASTE_LEARNING.to_owned()),
    ];
    // Omitted along with the threadId it names: a session the deployment did not
    // report is absent, not present and empty.
    if !context.session_id.is_empty() {
        headers.push(("x-session-id".to_owned(), context.session_id.clone()));
    }
    headers.push(("Authorization".to_owned(), format!("Bearer {}", context.api_key)));
    headers.push(("traceparent".to_owned(), context.traceparent.to_string()));
    if context.zdr {
        headers.push(("x-cmd-zdr".to_owned(), "1".to_owned()));
    }
    headers
}

/// Map a tool name to the name the upstream expects in a definition.
fn wire_tool_name(name: &str) -> &str {
    TOOL_NAME_ALIASES
        .iter()
        .find(|(from, _)| *from == name)
        .map_or(name, |(_, to)| to)
}

/// `id → name` for every tool call in the history, so tool results can carry a
/// name even when the client only sent an id.
fn tool_names(messages: &[CanonicalMessage]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for message in messages {
        for block in &message.content {
            if let ContentBlock::ToolUse { id, name, .. } = block {
                names.insert(id.clone(), name.clone());
            }
        }
    }
    names
}

fn cc_messages(messages: &[CanonicalMessage]) -> Vec<Value> {
    let names = tool_names(messages);
    messages.iter().map(|message| cc_message(message, &names)).collect()
}

fn cc_message(message: &CanonicalMessage, names: &HashMap<String, String>) -> Value {
    match message.role {
        Role::User => json!({
            "role": "user",
            "content": message.content.iter().filter_map(user_part).collect::<Vec<_>>(),
        }),
        Role::Assistant => json!({
            "role": "assistant",
            "content": message.content.iter().filter_map(assistant_part).collect::<Vec<_>>(),
        }),
        Role::Tool => json!({
            "role": "tool",
            "content": message.content.iter().filter_map(|block| tool_part(block, names)).collect::<Vec<_>>(),
        }),
    }
}

fn user_part(block: &ContentBlock) -> Option<Value> {
    match block {
        ContentBlock::Text { text, cache_control } => {
            let mut part = Map::new();
            part.insert("type".to_owned(), json!("text"));
            part.insert("text".to_owned(), json!(text));
            if let Some(cache) = cache_control {
                part.insert("cache_control".to_owned(), json!({ "type": cache.kind }));
            }
            Some(Value::Object(part))
        }
        ContentBlock::Image { source, .. } => {
            let mut part = Map::new();
            part.insert("type".to_owned(), json!("image"));
            match source {
                ImageSource::Base64 { media_type, data } => {
                    part.insert("image".to_owned(), json!(format!("data:{media_type};base64,{data}")));
                    part.insert("mimeType".to_owned(), json!(media_type));
                }
                ImageSource::Url { url, media_type } => {
                    part.insert("image".to_owned(), json!(url));
                    if let Some(media_type) = media_type {
                        part.insert("mimeType".to_owned(), json!(media_type));
                    }
                }
            }
            Some(Value::Object(part))
        }
        // Already in wire shape: rebuilding it is what would lose the client's
        // own fields.
        ContentBlock::Opaque { value } => Some(value.clone()),
        _ => None,
    }
}

/// Reasoning first, then text, then tool calls: the order the upstream expects.
fn assistant_part(block: &ContentBlock) -> Option<Value> {
    match block {
        ContentBlock::Reasoning { text } => Some(json!({ "type": "reasoning", "text": text })),
        ContentBlock::Text { .. } => user_part(block),
        ContentBlock::ToolUse { id, name, input } => Some(json!({
            "type": "tool-call",
            "toolCallId": id,
            "toolName": name,
            "input": input,
        })),
        _ => None,
    }
}

fn tool_part(block: &ContentBlock, names: &HashMap<String, String>) -> Option<Value> {
    let ContentBlock::ToolResult {
        tool_use_id,
        name,
        content,
        ..
    } = block
    else {
        return None;
    };
    let tool_name = names
        .get(tool_use_id)
        .cloned()
        .or_else(|| name.clone())
        .unwrap_or_default();
    Some(json!({
        "type": "tool-result",
        "toolCallId": tool_use_id,
        "toolName": tool_name,
        "output": { "type": "text", "value": content },
    }))
}

fn cc_tools(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "name": wire_tool_name(&tool.name),
                "description": tool.description.clone().unwrap_or_default(),
                "input_schema": tool
                    .parameters
                    .clone()
                    .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
            })
        })
        .collect()
}

fn cc_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!({ "type": "auto" }),
        ToolChoice::None => json!({ "type": "none" }),
        ToolChoice::Required => json!({ "type": "any" }),
        ToolChoice::Named { name } => json!({ "type": "tool", "name": name }),
    }
}
