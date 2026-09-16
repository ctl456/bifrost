//! Fidelity check against the original proxy.
//!
//! `fixtures/responses_params_golden.json` is produced by executing
//! `convertResponsesToChat` and `buildCcRequest` from
//! `commandcode-proxy/proxy.mjs` verbatim (see the header of that file). This test
//! runs the whole Bifrost path — Responses decode, then `cc/1.53.1` encode — and
//! requires the resulting `params` object to be identical, so a change to either
//! layer that alters the wire shape fails here rather than in production.
//!
//! The original refuses two shapes outright, which is asserted below rather than
//! carried in a fixture: a fixture records what the original *produces*, and a
//! refusal produces a status, not a request.

use bifrost::fingerprint::DeviceProfile;
use bifrost::protocol::adapter::{ConvertOptions, ProtocolAdapter};
use bifrost::protocol::openai::responses::Responses;
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
    serde_json::from_str(include_str!("../fixtures/responses_params_golden.json")).expect("golden fixture parses")
}

/// Decode a client body and encode it for the upstream.
fn convert(request: &Value) -> Value {
    let body = serde_json::to_vec(request).expect("serialize fixture request");
    let decoded = Responses
        .decode_request(&body, &ConvertOptions::default())
        .unwrap_or_else(|error| panic!("decode failed: {error}"));
    let wire = CcV1531::default().encode(&decoded.value, &context()).expect("encode");
    wire.body.expect("a generate carries a body")
}

#[test]
fn every_golden_case_produces_identical_params() {
    let fixture = golden();
    let cases = fixture["cases"].as_array().expect("cases");
    assert!(cases.len() >= 12, "the fixture must stay representative");

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
fn a_response_id_from_an_earlier_turn_is_refused_not_ignored() {
    // Honouring it is impossible without server-side state and ignoring it would
    // answer a follow-up as if it were the first question, so the request fails.
    let error = Responses
        .decode_request(
            br#"{"input":"hi","previous_response_id":"resp_1"}"#,
            &ConvertOptions::default(),
        )
        .expect_err("must refuse");
    assert_eq!(error.status_code(), 400);
    assert!(error.to_string().contains("previous_response_id"), "{error}");
}

#[test]
fn a_request_with_no_input_is_refused() {
    let error = Responses
        .decode_request(br#"{"model":"m"}"#, &ConvertOptions::default())
        .expect_err("must refuse");
    assert_eq!(error.status_code(), 400);
    assert!(error.to_string().contains("input is required"), "{error}");
}

#[test]
fn instructions_alone_are_an_input() {
    // The original counts the prompt and the conversation in one list before
    // splitting them, so `instructions` alone passes its emptiness check. A port
    // that counted only the conversation would refuse a request the original
    // answers, and the client would see a 400 for a working call.
    let decoded = Responses
        .decode_request(br#"{"instructions":"be brief","input":[]}"#, &ConvertOptions::default())
        .expect("decode");
    assert_eq!(decoded.value.system.len(), 1);
    assert!(decoded.value.messages.is_empty());
}

#[test]
fn an_item_that_only_named_a_role_is_a_message() {
    // `type` is optional on the message arm of the input union; reading such an
    // item as anything else drops the turn while the request still succeeds.
    let decoded = Responses
        .decode_request(
            br#"{"input":[{"role":"user","content":"hi"}]}"#,
            &ConvertOptions::default(),
        )
        .expect("decode");
    assert_eq!(decoded.value.messages.len(), 1);
    assert_eq!(decoded.value.messages[0].content[0].as_text(), Some("hi"));
}
