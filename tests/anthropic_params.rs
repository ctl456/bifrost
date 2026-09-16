//! Fidelity check against the original proxy.
//!
//! `fixtures/anthropic_params_golden.json` is produced by executing
//! `convertAnthropicToOpenAI` and `buildCcRequest` from
//! `commandcode-proxy/proxy.mjs` verbatim (see the header of that file). This test
//! runs the whole Bifrost path — Messages decode, then `cc/1.53.1` encode — and
//! requires the resulting `params` object to be identical, so a change to either
//! layer that alters the wire shape fails here rather than in production.
//!
//! Two intentional departures from the original, neither of which any case below
//! exercises because the original's behavior is not reproducible or not sane:
//!
//! - A `tool_use` block with no id gets a positional one (`call_0`) where the
//!   original forwarded no id at all.
//! - A forced `tool_choice` with no name falls back to `auto` where the original
//!   sent `{"type":"tool"}` with the name missing.

use bifrost::fingerprint::DeviceProfile;
use bifrost::protocol::adapter::{ConvertOptions, ProtocolAdapter};
use bifrost::protocol::anthropic::AnthropicMessages;
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
    serde_json::from_str(include_str!("../fixtures/anthropic_params_golden.json")).expect("golden fixture parses")
}

/// Decode a client body and encode it for the upstream.
fn convert(request: &Value) -> Value {
    let body = serde_json::to_vec(request).expect("serialize fixture request");
    let decoded = AnthropicMessages
        .decode_request(&body, &ConvertOptions::default())
        .unwrap_or_else(|error| panic!("decode failed: {error}"));
    let wire = CcV1531::default().encode(&decoded.value, &context()).expect("encode");
    wire.body.expect("a generate carries a body")
}

#[test]
fn every_golden_case_produces_identical_params() {
    let fixture = golden();
    let cases = fixture["cases"].as_array().expect("cases");
    assert!(cases.len() >= 15, "the fixture must stay representative");

    for case in cases {
        let name = case["name"].as_str().expect("case name");
        let expected = &case["params"];
        let actual = convert(&case["request"]);
        assert_eq!(
            &actual["params"], expected,
            "params diverged for case `{name}`\n  proxy:   {expected}\n  bifrost: {}",
            actual["params"]
        );
    }
}

#[test]
fn a_prompt_the_client_sent_verbatim_reaches_the_wire_verbatim() {
    let fixture = golden();
    let case = fixture["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|case| {
            case["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("a thinking turn"))
        })
        .expect("the reasoning case exists");
    let params = convert(&case["request"]);
    let messages = params["params"]["messages"].as_array().expect("messages");

    // The turn's reasoning leads, its text follows, then its calls: the order the
    // upstream validates, whatever order the client used.
    assert_eq!(
        messages[1],
        serde_json::json!({
            "role": "assistant",
            "content": [
                { "type": "reasoning", "text": "I should search" },
                { "type": "text", "text": "on it" },
                { "type": "tool-call", "toolCallId": "toolu_1", "toolName": "search", "input": { "q": "cats" } },
                { "type": "tool-call", "toolCallId": "toolu_2", "toolName": "fetch", "input": {} },
            ],
        })
    );
    // The result is its own turn, ahead of the text it arrived with.
    assert_eq!(
        messages[2],
        serde_json::json!({
            "role": "tool",
            "content": [{ "type": "tool-result", "toolCallId": "toolu_1", "toolName": "search", "output": { "type": "text", "value": "found 3" } }],
        })
    );
    assert_eq!(
        messages[3],
        serde_json::json!({
            "role": "user",
            "content": [{ "type": "text", "text": "and now?" }],
        })
    );
}

#[test]
fn a_prompt_with_no_usable_section_gets_the_placeholder_instead() {
    let fixture = golden();
    let case = fixture["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|case| {
            case["name"]
                .as_str()
                .is_some_and(|name| name.contains("empty prompt section"))
        })
        .expect("the empty-prompt case exists");
    assert_eq!(
        convert(&case["request"])["params"]["system"],
        serde_json::json!([{ "type": "text", "text": " " }]),
        "an empty prompt must not leave the key out, or the upstream injects its own"
    );
}
