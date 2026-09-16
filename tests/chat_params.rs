//! Fidelity check against the original proxy.
//!
//! `fixtures/chat_params_golden.json` is produced by executing
//! `buildCcRequest` from `commandcode-proxy/proxy.mjs` verbatim (see the header
//! of that file). This test runs the *whole* Bifrost path — protocol decode, then
//! `cc/1.53.1` encode — and requires the resulting `params` object to be
//! identical, so a change to either layer that alters the wire shape fails here
//! rather than in production.
//!
//! One intentional departure, which no case below can carry because the fixture
//! is generated from the original and would disagree with it by construction:
//!
//! - `max_completion_tokens` is honoured. The original destructures `max_tokens`
//!   alone, so a client using the spelling the OpenAI SDK now emits silently
//!   receives the default cap instead of the one it asked for.

use bifrost::fingerprint::DeviceProfile;
use bifrost::protocol::adapter::{ConvertOptions, ProtocolAdapter};
use bifrost::protocol::openai::chat::ChatCompletions;
use bifrost::wire::cc::CcV1531;
use bifrost::wire::{SystemEntropy, WireAdapter, WireContext};
use serde_json::Value;

fn context() -> WireContext {
    WireContext::new(
        "user_golden",
        "3f2504e0-4f89-11d3-9a0c-0305e82c3301",
        DeviceProfile::default(),
        "2026-01-01",
        &SystemEntropy::new(),
    )
}

fn golden() -> Value {
    serde_json::from_str(include_str!("../fixtures/chat_params_golden.json")).expect("golden fixture parses")
}

/// Decode a client body and encode it for the upstream.
fn convert(request: &Value, synthesize_cache_breakpoint: bool) -> Value {
    let body = serde_json::to_vec(request).expect("serialize fixture request");
    let options = ConvertOptions {
        synthesize_cache_breakpoint,
    };
    let decoded = ChatCompletions
        .decode_request(&body, &options)
        .unwrap_or_else(|error| panic!("decode failed: {error}"));
    let wire = CcV1531::default().encode(&decoded.value, &context()).expect("encode");
    wire.body.expect("a generate carries a body")
}

#[test]
fn every_golden_case_produces_identical_params() {
    let fixture = golden();
    let cases = fixture["cases"].as_array().expect("cases");
    assert!(cases.len() >= 10, "the fixture must stay representative");

    for case in cases {
        let name = case["name"].as_str().expect("case name");
        let expected = &case["params"];
        let actual = convert(&case["request"], true);
        assert_eq!(
            &actual["params"], expected,
            "params diverged for case `{name}`\n  proxy:   {expected}\n  bifrost: {}",
            actual["params"]
        );
    }
}

#[test]
fn a_part_the_build_does_not_model_crosses_the_ir_untouched() {
    // The fixture case proves the bytes match the original; this pins the shape
    // on the IR side and the warning that tells an operator the part went through
    // unread, which is the only trace a silently dropped payload would not leave.
    let body = serde_json::json!({
        "messages": [{ "role": "user", "content": [{ "type": "video", "url": "x" }] }],
    })
    .to_string();
    let decoded = ChatCompletions
        .decode_request(body.as_bytes(), &ConvertOptions::default())
        .expect("decode");

    assert_eq!(
        decoded.value.messages[0].content,
        vec![bifrost::core::ContentBlock::opaque(
            serde_json::json!({ "type": "video", "url": "x" })
        )],
    );
    assert_eq!(decoded.warnings.len(), 1, "{:?}", decoded.warnings);
    assert_eq!(decoded.warnings[0].path, "messages[0].content[0].type");
    assert!(
        decoded.warnings[0].detail.contains("forwarded verbatim"),
        "{:?}",
        decoded.warnings[0]
    );
}

#[test]
fn the_newer_max_tokens_spelling_is_honoured_unlike_the_original() {
    // `convert` mirrors the fixture's path, so this asserts on the wire body
    // rather than on the IR: the point is which number the upstream is asked for.
    let body = convert(
        &serde_json::json!({ "messages": [{ "role": "user", "content": "hi" }], "max_completion_tokens": 256 }),
        true,
    );
    assert_eq!(body["params"]["max_tokens"], serde_json::json!(256));
}

#[test]
fn the_envelope_outside_params_is_unaffected_by_the_conversion() {
    let fixture = golden();
    let case = &fixture["cases"][0];
    let body = convert(&case["request"], true);
    let mut keys: Vec<&str> = body.as_object().expect("object").keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "config",
            "memory",
            "mode",
            "params",
            "permissionMode",
            "skills",
            "taste",
            "threadId"
        ],
    );
}

#[test]
fn turning_off_the_cache_mechanism_leaves_the_prompt_unmarked() {
    let fixture = golden();
    let case = fixture["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|case| {
            case["name"]
                .as_str()
                .is_some_and(|name| name.contains("prompt cache key"))
        })
        .expect("the cache-key case exists");

    let without = convert(&case["request"], false);
    assert_eq!(
        without["params"]["system"],
        serde_json::json!([{ "type": "text", "text": "stable prefix" }]),
        "a breakpoint must not be invented when the mechanism is off"
    );

    let with = convert(&case["request"], true);
    assert_eq!(
        with["params"]["system"],
        serde_json::json!([{ "type": "text", "text": "stable prefix", "cache_control": { "type": "ephemeral" } }]),
    );
}
