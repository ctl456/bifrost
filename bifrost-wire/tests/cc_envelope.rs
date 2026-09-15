//! The `cc/1.53.1` envelope and header set.
//!
//! The envelope is compared as serialized text, not as a parsed value: object
//! key order is part of the wire contract, and a value comparison would not
//! notice a reordering.

use bifrost_config::WireConfig;
use bifrost_core::{
    CacheControl, CanonicalMessage, CanonicalRequest, ContentBlock, ImageSource, ReasoningEffort, Role, ToolChoice,
    ToolDefinition,
};
use bifrost_fingerprint::DeviceProfile;
use bifrost_wire::cc::CcV1531;
use bifrost_wire::{SUPPORTED, SequenceEntropy, WireAdapter, WireContext, adapter_for};

const SESSION: &str = "11111111-2222-4333-8444-555555555555";

fn adapter() -> CcV1531 {
    CcV1531::default()
}

fn context() -> WireContext {
    WireContext::new(
        "user_abc123",
        SESSION,
        DeviceProfile::default(),
        "2026-09-15",
        &SequenceEntropy::new(7),
    )
}

fn encode(request: &CanonicalRequest) -> serde_json::Value {
    adapter()
        .encode(request, &context())
        .expect("encodes")
        .body
        .expect("a generate carries a body")
}

#[test]
fn a_minimal_request_matches_the_expected_envelope_exactly() {
    let mut request = CanonicalRequest::new(vec![CanonicalMessage::user("hi")]);
    request.model = Some("deepseek/deepseek-v4-flash".to_owned());

    let actual = encode(&request).to_string();
    let expected = concat!(
        r#"{"config":{"workingDir":"C:\\Users\\dev\\projects\\app","date":"2026-09-15","environment":"win32","#,
        r#""structure":[],"isGitRepo":false,"currentBranch":"","mainBranch":"","gitStatus":"","recentCommits":[]},"#,
        r#""memory":null,"taste":null,"skills":null,"permissionMode":"standard","#,
        r#""threadId":"11111111-2222-4333-8444-555555555555","mode":"agent","#,
        r#""params":{"model":"deepseek/deepseek-v4-flash","#,
        r#""messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}],"#,
        r#""max_tokens":64000,"stream":true,"system":[{"type":"text","text":" "}],"tools":[]}}"#,
    );
    assert_eq!(actual, expected);
}

#[test]
fn the_envelope_keys_keep_the_client_order() {
    let request = CanonicalRequest::new(vec![CanonicalMessage::user("hi")]);
    let body = encode(&request);
    let keys: Vec<&str> = body.as_object().expect("object").keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "config",
            "memory",
            "taste",
            "skills",
            "permissionMode",
            "threadId",
            "mode",
            "params"
        ]
    );
}

#[test]
fn a_session_that_is_not_a_uuid_omits_thread_id() {
    let mut context = context();
    context.session_id = "not-a-uuid".to_owned();
    let request = CanonicalRequest::new(vec![CanonicalMessage::user("hi")]);
    let body = adapter()
        .encode(&request, &context)
        .expect("encodes")
        .body
        .expect("a generate carries a body");

    let object = body.as_object().expect("object");
    assert!(
        !object.contains_key("threadId"),
        "a malformed thread id must be omitted, not sent"
    );
    assert!(
        object.get("params").is_some(),
        "the rest of the envelope must survive the omission"
    );
    assert_eq!(body.pointer("/params/stream"), Some(&serde_json::json!(true)));
}

#[test]
fn the_session_is_reported_as_a_header() {
    let request = CanonicalRequest::new(vec![CanonicalMessage::user("hi")]);
    let encoded = adapter().encode(&request, &context()).expect("encodes");
    assert_eq!(encoded.header("x-session-id"), Some(SESSION));
    assert_eq!(encoded.header("authorization"), Some("Bearer user_abc123"));
}

/// A deployment that reports no session sends none of it: the header and the
/// threadId are the same fact, and a header naming nothing would be a third
/// version of it.
#[test]
fn a_deployment_without_a_session_omits_both() {
    let mut context = context();
    context.session_id = String::new();
    let request = CanonicalRequest::new(vec![CanonicalMessage::user("hi")]);
    let encoded = adapter().encode(&request, &context).expect("encodes");

    assert_eq!(encoded.header("x-session-id"), None);
    let body = encoded.body.expect("a generate carries a body");
    assert!(
        !body.as_object().expect("object").contains_key("threadId"),
        "no session means no thread id"
    );
    let names: Vec<&str> = encoded.headers.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Content-Type",
            "User-Agent",
            "x-command-code-version",
            "x-cli-environment",
            "x-project-slug",
            "x-taste-learning",
            "Authorization",
            "traceparent",
        ]
    );
}

#[test]
fn the_header_set_matches_the_client() {
    let request = CanonicalRequest::new(vec![CanonicalMessage::user("hi")]);
    let encoded = adapter().encode(&request, &context()).expect("encodes");
    let names: Vec<&str> = encoded.headers.iter().map(|(name, _)| name.as_str()).collect();

    assert_eq!(
        names,
        [
            "Content-Type",
            "User-Agent",
            "x-command-code-version",
            "x-cli-environment",
            "x-project-slug",
            "x-taste-learning",
            "x-session-id",
            "Authorization",
            "traceparent",
        ]
    );
    assert_eq!(encoded.header("User-Agent"), Some("cli"));
    assert_eq!(encoded.header("x-command-code-version"), Some("1.53.1"));
    assert_eq!(encoded.header("x-cli-environment"), Some("production"));
    assert_eq!(encoded.header("x-taste-learning"), Some("false"));
    assert_eq!(encoded.header("x-project-slug"), Some("c-users-dev-projects-app"));
}

#[test]
fn zdr_is_opt_in_and_appends_the_flag_header() {
    let request = CanonicalRequest::new(vec![CanonicalMessage::user("hi")]);
    let plain = adapter().encode(&request, &context()).expect("encodes");
    assert_eq!(plain.header("x-cmd-zdr"), None);

    let zdr = adapter().encode(&request, &context().with_zdr(true)).expect("encodes");
    assert_eq!(zdr.header("x-cmd-zdr"), Some("1"));
}

#[test]
fn an_empty_system_prompt_gets_a_placeholder() {
    let request = CanonicalRequest::new(vec![CanonicalMessage::user("hi")]);
    assert_eq!(
        encode(&request).pointer("/params/system"),
        Some(&serde_json::json!([{ "type": "text", "text": " " }]))
    );

    let without = CcV1531 {
        empty_system_placeholder: false,
        ..CcV1531::default()
    };
    let body = without
        .encode(&request, &context())
        .expect("encodes")
        .body
        .expect("a generate carries a body");
    assert_eq!(
        body.pointer("/params/system"),
        None,
        "disabling the placeholder must omit the key"
    );
}

#[test]
fn system_sections_are_joined_and_keep_their_cache_breakpoint() {
    let mut request = CanonicalRequest::new(vec![CanonicalMessage::user("hi")]);
    request.system = vec![
        ContentBlock::text("first"),
        ContentBlock::Text {
            text: "second".to_owned(),
            cache_control: Some(CacheControl::ephemeral()),
        },
    ];
    let system = encode(&request).pointer("/params/system").cloned().expect("system");
    assert_eq!(
        system,
        serde_json::json!([
            { "type": "text", "text": "first\n" },
            { "type": "text", "text": "second", "cache_control": { "type": "ephemeral" } }
        ])
    );
}

#[test]
fn tools_are_always_present_even_when_empty() {
    let request = CanonicalRequest::new(vec![CanonicalMessage::user("hi")]);
    assert_eq!(encode(&request).pointer("/params/tools"), Some(&serde_json::json!([])));
}

#[test]
fn tool_definitions_are_aliased_and_defaulted() {
    let mut request = CanonicalRequest::new(vec![CanonicalMessage::user("hi")]);
    request.tools = vec![
        ToolDefinition {
            name: "bash_output".to_owned(),
            description: None,
            parameters: None,
        },
        ToolDefinition {
            name: "read_file".to_owned(),
            description: Some("read a file".to_owned()),
            parameters: Some(serde_json::json!({ "type": "object", "properties": { "path": { "type": "string" } } })),
        },
    ];
    let tools = encode(&request).pointer("/params/tools").cloned().expect("tools");
    assert_eq!(
        tools,
        serde_json::json!([
            { "name": "shell_output", "description": "", "input_schema": { "type": "object", "properties": {} } },
            {
                "name": "read_file",
                "description": "read a file",
                "input_schema": { "type": "object", "properties": { "path": { "type": "string" } } }
            }
        ])
    );
}

#[test]
fn tool_choice_is_translated_and_an_absent_one_is_left_out() {
    let cases = [
        (None, None, "no choice sent"),
        (
            Some(ToolChoice::Auto),
            Some(serde_json::json!({ "type": "auto" })),
            "explicit auto",
        ),
        (
            Some(ToolChoice::None),
            Some(serde_json::json!({ "type": "none" })),
            "none",
        ),
        (
            Some(ToolChoice::Required),
            Some(serde_json::json!({ "type": "any" })),
            "required",
        ),
        (
            Some(ToolChoice::named("apply_patch")),
            Some(serde_json::json!({ "type": "tool", "name": "apply_patch" })),
            "named",
        ),
    ];
    for (choice, expected, label) in cases {
        let mut request = CanonicalRequest::new(vec![CanonicalMessage::user("hi")]);
        request.tool_choice = choice;
        assert_eq!(
            encode(&request).pointer("/params/tool_choice").cloned(),
            expected,
            "choice: {label}"
        );
    }
}

#[test]
fn max_tokens_defaults_and_is_capped() {
    let mut request = CanonicalRequest::new(vec![CanonicalMessage::user("hi")]);
    assert_eq!(
        encode(&request).pointer("/params/max_tokens"),
        Some(&serde_json::json!(64_000))
    );

    request.params.max_tokens = Some(512);
    assert_eq!(
        encode(&request).pointer("/params/max_tokens"),
        Some(&serde_json::json!(512))
    );

    request.params.max_tokens = Some(500_000);
    assert_eq!(
        encode(&request).pointer("/params/max_tokens"),
        Some(&serde_json::json!(200_000))
    );
}

#[test]
fn the_upstream_is_always_asked_to_stream() {
    let mut request = CanonicalRequest::new(vec![CanonicalMessage::user("hi")]);
    request.stream = false;
    assert_eq!(
        encode(&request).pointer("/params/stream"),
        Some(&serde_json::json!(true))
    );
}

#[test]
fn optional_params_appear_only_when_set() {
    let request = CanonicalRequest::new(vec![CanonicalMessage::user("hi")]);
    let body = encode(&request);
    assert_eq!(body.pointer("/params/temperature"), None);
    assert_eq!(body.pointer("/params/reasoning_effort"), None);
    assert_eq!(body.pointer("/params/parallel_tool_calls"), None);

    let mut request = CanonicalRequest::new(vec![CanonicalMessage::user("hi")]);
    request.params.temperature = Some(0.25);
    request.params.reasoning_effort = Some(ReasoningEffort::Max);
    request.parallel_tool_calls = Some(false);
    let body = encode(&request);
    assert_eq!(body.pointer("/params/temperature"), Some(&serde_json::json!(0.25)));
    assert_eq!(
        body.pointer("/params/reasoning_effort"),
        Some(&serde_json::json!("max"))
    );
    assert_eq!(
        body.pointer("/params/parallel_tool_calls"),
        Some(&serde_json::json!(false))
    );
}

#[test]
fn assistant_history_keeps_reasoning_text_and_calls_in_order() {
    let mut request = CanonicalRequest::new(vec![CanonicalMessage::new(
        Role::Assistant,
        vec![
            ContentBlock::reasoning("thinking"),
            ContentBlock::text("answer"),
            ContentBlock::ToolUse {
                id: "call_1".to_owned(),
                name: "get_weather".to_owned(),
                input: serde_json::json!({ "city": "Beijing" }),
            },
        ],
    )]);

    let messages = encode(&request).pointer("/params/messages").cloned().expect("messages");
    assert_eq!(
        messages,
        serde_json::json!([{
            "role": "assistant",
            "content": [
                { "type": "reasoning", "text": "thinking" },
                { "type": "text", "text": "answer" },
                { "type": "tool-call", "toolCallId": "call_1", "toolName": "get_weather", "input": { "city": "Beijing" } }
            ]
        }])
    );

    // The reverse map is built from assistant messages.
    request.messages.push(CanonicalMessage::new(
        Role::Tool,
        vec![ContentBlock::ToolResult {
            tool_use_id: "call_1".to_owned(),
            name: None,
            content: "22C".to_owned(),
            is_error: false,
        }],
    ));
    let messages = encode(&request).pointer("/params/messages").cloned().expect("messages");
    assert_eq!(
        messages.pointer("/1"),
        Some(&serde_json::json!({
            "role": "tool",
            "content": [{
                "type": "tool-result",
                "toolCallId": "call_1",
                "toolName": "get_weather",
                "output": { "type": "text", "value": "22C" }
            }]
        }))
    );
}

#[test]
fn a_tool_result_falls_back_to_its_own_name_when_no_call_matches() {
    let request = CanonicalRequest::new(vec![CanonicalMessage::new(
        Role::Tool,
        vec![ContentBlock::ToolResult {
            tool_use_id: "orphan".to_owned(),
            name: Some("bash".to_owned()),
            content: "done".to_owned(),
            is_error: false,
        }],
    )]);
    assert_eq!(
        encode(&request).pointer("/params/messages/0/content/0/toolName"),
        Some(&serde_json::json!("bash"))
    );
}

#[test]
fn base64_images_become_data_urls_with_a_mime_type() {
    let request = CanonicalRequest::new(vec![CanonicalMessage::new(
        Role::User,
        vec![
            ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/jpeg".to_owned(),
                    data: "AAAA".to_owned(),
                },
                cache_control: None,
            },
            ContentBlock::text("what is this"),
        ],
    )]);
    assert_eq!(
        encode(&request).pointer("/params/messages/0/content").cloned(),
        Some(serde_json::json!([
            { "type": "image", "image": "data:image/jpeg;base64,AAAA", "mimeType": "image/jpeg" },
            { "type": "text", "text": "what is this" }
        ]))
    );
}

#[test]
fn url_images_are_passed_through_without_a_mime_type() {
    let request = CanonicalRequest::new(vec![CanonicalMessage::new(
        Role::User,
        vec![ContentBlock::Image {
            source: ImageSource::Url {
                url: "https://example.test/a.png".to_owned(),
                media_type: None,
            },
            cache_control: None,
        }],
    )]);
    assert_eq!(
        encode(&request).pointer("/params/messages/0/content/0").cloned(),
        Some(serde_json::json!({ "type": "image", "image": "https://example.test/a.png" }))
    );
}

#[test]
fn the_request_targets_the_generate_endpoint_on_the_configured_base() {
    let request = CanonicalRequest::new(vec![CanonicalMessage::user("hi")]);
    let encoded = adapter().encode(&request, &context()).expect("encodes");
    assert_eq!(encoded.method, "POST");
    assert_eq!(encoded.path, "/alpha/generate");
    assert_eq!(
        encoded.url("https://api.commandcode.ai"),
        "https://api.commandcode.ai/alpha/generate"
    );
    assert_eq!(
        encoded.url("https://api.commandcode.ai/"),
        "https://api.commandcode.ai/alpha/generate"
    );
}

/// The upstream's edge answers a request that carries no `User-Agent` with
/// `403 error code: 1010`, before it looks at the path. Every request has to
/// carry one, and the announcement set is where it was missing: a generate had it
/// and nothing else did, which left the lifecycle pre-flight and the model
/// catalogue failing against the live service while the tests stayed green —
/// because nothing pinned those two sets at all.
#[test]
fn every_request_carries_a_user_agent() {
    let request = CanonicalRequest::new(vec![CanonicalMessage::user("hi")]);
    let generated = adapter().encode(&request, &context()).expect("encodes");
    assert_eq!(generated.header("user-agent"), Some("cli"));

    let announced = adapter().announce(&context(), None, &SequenceEntropy::new(7));
    assert_eq!(announced.len(), 1, "no fingerprint means the event alone");
    assert_eq!(announced[0].header("user-agent"), Some("cli"));

    let catalogue = adapter()
        .provider_models("user_abc123")
        .expect("the catalogue is a request");
    assert_eq!(catalogue.header("user-agent"), Some("cli"));
    assert_eq!(catalogue.header("authorization"), Some("Bearer user_abc123"));
}

/// The announcement set, which carries the device record and the session event.
/// Pinned by name and order because it is the set that silently lost a header.
#[test]
fn the_announcement_header_set_matches_the_client() {
    let announced = adapter().announce(&context(), None, &SequenceEntropy::new(7));
    let names: Vec<&str> = announced[0].headers.iter().map(|(name, _)| name.as_str()).collect();

    assert_eq!(
        names,
        [
            "Content-Type",
            "User-Agent",
            "x-command-code-version",
            "x-cli-environment",
            "Authorization",
        ]
    );
    assert_eq!(announced[0].header("x-command-code-version"), Some("1.53.1"));
    assert_eq!(announced[0].header("x-cli-environment"), Some("production"));
    // Narrower than a generate on purpose: no directory, no session, no trace.
    assert_eq!(announced[0].header("traceparent"), None);
    assert_eq!(announced[0].header("x-session-id"), None);
    assert_eq!(announced[0].header("x-project-slug"), None);
}

/// The device record and the session event go out as one pair, and both are told
/// the same thing about who is calling.
#[test]
fn a_fingerprint_rides_alongside_the_event_with_the_same_headers() {
    let fingerprint = bifrost_fingerprint::generate_fingerprint("user_abc123", "salt", &DeviceProfile::default());
    let announced = adapter().announce(&context(), Some(&fingerprint), &SequenceEntropy::new(7));

    assert_eq!(announced.len(), 2);
    assert_eq!(announced[0].path, "/alpha/fingerprint/record");
    assert_eq!(announced[1].path, "/alpha/lifecycle-events");
    assert_eq!(announced[0].headers, announced[1].headers);
    assert_eq!(announced[0].header("user-agent"), Some("cli"));
}

/// ZDR is opt-in and rides on the announcement too, appended rather than
/// reordered, so the set above stays checkable.
#[test]
fn the_announcement_carries_zdr_only_when_asked() {
    let plain = adapter().announce(&context(), None, &SequenceEntropy::new(7));
    assert_eq!(plain[0].header("x-cmd-zdr"), None);

    let zdr = adapter().announce(&context().with_zdr(true), None, &SequenceEntropy::new(7));
    assert_eq!(zdr[0].header("x-cmd-zdr"), Some("1"));
}

/// The catalogue is a `GET` of an account's models, so it takes the announcement's
/// headers and none of the body a generate needs.
#[test]
fn the_catalogue_is_a_get_with_the_announcement_headers() {
    let catalogue = adapter()
        .provider_models("user_abc123")
        .expect("the catalogue is a request");
    let names: Vec<&str> = catalogue.headers.iter().map(|(name, _)| name.as_str()).collect();

    assert_eq!(
        names,
        [
            "Content-Type",
            "User-Agent",
            "x-command-code-version",
            "x-cli-environment",
            "Authorization",
        ]
    );
    assert_eq!(catalogue.method, "GET");
    assert_eq!(catalogue.path, "/provider/v1/models");
    assert_eq!(catalogue.body, None);
}

#[test]
fn the_adapter_reports_its_identity() {
    let adapter = adapter();
    assert_eq!(adapter.name(), "command-code");
    assert_eq!(adapter.version(), "1.53.1");
    assert_eq!(adapter.id(), "command-code/1.53.1");
}

/// The dialect knows what it was read from, which is what a drift check needs
/// and the only thing that ties a version on a registry to this implementation.
#[test]
fn the_dialect_names_the_package_it_was_read_from() {
    assert_eq!(adapter().published_package(), Some("command-code"));
}

#[test]
fn adapter_lookup_accepts_the_supported_version_and_refuses_others() {
    assert_eq!(SUPPORTED, ["cc/1.53.1"]);

    let config = WireConfig::default();
    let resolved = adapter_for(&config.parse_adapter().expect("parses")).expect("cc/1.53.1 resolves");
    assert_eq!(resolved.id(), "command-code/1.53.1");

    for configured in ["cc/1.54.0", "cc/1.53.0", "other/1.53.1"] {
        let config = WireConfig {
            adapter: configured.to_owned(),
            ..WireConfig::default()
        };
        assert!(
            adapter_for(&config.parse_adapter().expect("parses")).is_none(),
            "{configured} must not resolve"
        );
    }
}
