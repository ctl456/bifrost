//! Fidelity check for the complete (non-streaming) Responses body.
//!
//! `fixtures/responses_body_golden.json` is produced by executing
//! `buildResponsesObject` from `commandcode-proxy/proxy.mjs` verbatim (see the
//! header of that file). This test runs the whole Bifrost path — Responses decode,
//! upstream wire decode, accumulation, then the body — and requires the resulting
//! object to be identical, key order included, because the JSON *string* is what
//! the client parses.

use bifrost::core::OutputAccumulator;
use bifrost::fingerprint::DeviceProfile;
use bifrost::protocol::adapter::{ConvertOptions, ProtocolAdapter, ResponseMeta};
use bifrost::protocol::openai::responses::Responses;
use bifrost::protocol::openai::responses::response::response_body;
use bifrost::wire::cc::CcDecoder;
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
    serde_json::from_str(include_str!("../fixtures/responses_body_golden.json")).expect("golden fixture parses")
}

fn lines_of(case: &Value) -> Vec<String> {
    case["lines"]
        .as_array()
        .expect("lines")
        .iter()
        .map(|line| line.as_str().expect("line").to_owned())
        .collect()
}

/// Fold an upstream event stream into the response a complete body renders from.
fn accumulate(lines: &[String]) -> OutputAccumulator {
    let mut decoder = CcDecoder::new();
    let mut accumulator = OutputAccumulator::new();
    for line in lines {
        accumulator.extend(&decoder.decode_line(line).chunks);
    }
    accumulator
}

#[test]
fn every_golden_case_produces_an_identical_body() {
    let fixture = golden();
    let model = fixture["model"].as_str().expect("model");
    let id = fixture["id"].as_str().expect("id");
    let created = fixture["created"].as_i64().expect("created");
    let cases = fixture["cases"].as_array().expect("cases");
    assert!(cases.len() >= 5, "the fixture must stay representative");

    for case in cases {
        let name = case["name"].as_str().expect("case name");

        let body = serde_json::to_vec(&case["request"]).expect("serialize fixture request");
        let request = Responses
            .decode_request(&body, &ConvertOptions::default())
            .unwrap_or_else(|error| panic!("decode failed: {error}"));
        let response = accumulate(&lines_of(case)).into_response(id, model, created);
        let meta = ResponseMeta {
            id: id.to_owned(),
            model: model.to_owned(),
            created,
            completed: fixture["completed"].as_i64().expect("completed"),
        };

        let actual = response_body(&meta, &request.value, &response);
        assert_eq!(
            actual, case["response"],
            "the body diverged for `{name}`\n  proxy:   {}\n  bifrost: {actual}",
            case["response"]
        );
    }
}

#[test]
fn the_tool_choice_echo_is_a_mode_name_or_the_default() {
    // The endpoint reports what it was told to prefer, and an object-shaped
    // choice is reported as `auto` rather than as the object: the original does
    // the same, because its echo only keeps a string.
    let body = serde_json::to_vec(&serde_json::json!({
        "input": "hi",
        "tool_choice": { "type": "function", "name": "search" },
    }))
    .expect("serialize");
    let request = Responses
        .decode_request(&body, &ConvertOptions::default())
        .expect("decode");

    assert_eq!(
        request.value.echo.as_ref().expect("echo")["tool_choice"],
        serde_json::json!("auto")
    );
    // The choice itself still reaches the upstream; only the echo flattens.
    assert_eq!(
        CcV1531::default()
            .encode(&request.value, &context())
            .expect("encode")
            .body
            .expect("a generate carries a body")["params"]["tool_choice"],
        serde_json::json!({ "type": "tool", "name": "search" })
    );
}

use bifrost::wire::cc::CcV1531;
