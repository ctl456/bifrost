//! Fidelity check for the outbound stream, against the original proxy.
//!
//! `fixtures/chat_sse_golden.json` is produced by executing `createSseTranslator`
//! from `commandcode-proxy/proxy.mjs` verbatim and recording the frames it emits
//! for a set of upstream event streams. This test feeds the same lines through
//! Bifrost's decoder and renderer and requires the frames to be byte-identical —
//! key order included, because the JSON *string* is what the client sees.

use bifrost_protocol::openai::chat::ChatStreamRenderer;
use bifrost_wire::cc::CcDecoder;
use serde_json::Value;

/// Drives one golden case and returns the frames as strings.
fn frames(lines: &[&str]) -> (Vec<String>, CcDecoder) {
    let mut decoder = CcDecoder::new();
    let mut renderer = renderer();
    let mut frames = Vec::new();
    for line in lines {
        let decoded = decoder.decode_line(line);
        for chunk in &decoded.chunks {
            for frame in renderer.render(chunk) {
                frames.push(frame.as_str().to_owned());
            }
        }
    }
    (frames, decoder)
}

fn renderer() -> ChatStreamRenderer {
    ChatStreamRenderer::new(
        FIXTURE["model"].as_str().expect("model"),
        FIXTURE["id"].as_str().expect("id"),
        FIXTURE["created"].as_i64().expect("created"),
    )
}

static FIXTURE: std::sync::LazyLock<Value> = std::sync::LazyLock::new(|| {
    serde_json::from_str(include_str!("../fixtures/chat_sse_golden.json")).expect("golden fixture parses")
});

#[test]
fn every_golden_stream_produces_identical_frames() {
    let cases = FIXTURE["cases"].as_array().expect("cases");
    assert!(cases.len() >= 5, "the fixture must stay representative");

    for case in cases {
        let name = case["name"].as_str().expect("case name");
        let lines: Vec<&str> = case["lines"]
            .as_array()
            .expect("lines")
            .iter()
            .map(|line| line.as_str().expect("line"))
            .collect();
        let (actual, decoder) = frames(&lines);

        let expected: Vec<&str> = case["frames"]
            .as_array()
            .expect("frames")
            .iter()
            .map(|frame| frame.as_str().expect("frame"))
            .collect();
        assert_eq!(
            actual, expected,
            "frames diverged for case `{name}`\n  proxy:   {expected:#?}\n  bifrost: {actual:#?}"
        );

        // The parser's own accounting is what the empty-response guard reads.
        assert_eq!(
            decoder.usage().completion_tokens as u64,
            case["output_tokens"].as_u64().expect("output_tokens"),
            "output tokens diverged for `{name}`"
        );
    }
}

#[test]
fn a_clean_stream_ends_with_the_done_sentinel() {
    let done = ChatStreamRenderer::done_frame();
    assert_eq!(done.as_str(), FIXTURE["cases"][0]["done"].as_str().expect("done"));
}

#[test]
fn a_missing_tool_call_id_gets_a_positional_one_not_a_timestamp() {
    // The original synthesizes `call_<Date.now()>_<n>`, which makes its output
    // unreproducible and changes whenever the process clock moves. Bifrost uses
    // the call's position instead: ids only have to be unique within a response
    // and to survive being echoed back, and a stable id means a stream can be
    // replayed byte for byte.
    let mut decoder = CcDecoder::new();
    let mut renderer = ChatStreamRenderer::new("m", "chatcmpl-fixture", 1_700_000_000);
    let decoded = decoder.decode_line(r#"{"type":"tool-call","toolName":"shell_output","input":{"cmd":"ls"}}"#);
    let frames: Vec<String> = decoded
        .chunks
        .iter()
        .flat_map(|chunk| renderer.render(chunk))
        .map(|frame| frame.as_str().to_owned())
        .collect();

    assert_eq!(frames.len(), 1);
    assert!(frames[0].contains(r#""id":"call_0""#), "{}", frames[0]);
    assert!(
        !frames[0].contains("_17"),
        "the id must not embed a timestamp: {}",
        frames[0]
    );
}

#[test]
fn the_renderer_commits_only_when_something_visible_is_sent() {
    let mut decoder = CcDecoder::new();
    let mut renderer = ChatStreamRenderer::new("m", "chatcmpl-fixture", 1_700_000_000);

    // Accounting and silent bookkeeping must not commit the response, or the
    // edge could no longer answer a zero-output request with a JSON 429.
    for line in [
        "{\"type\":\"start\"}",
        "{\"type\":\"provider-metadata\"}",
        "{\"type\":\"text-start\"}",
    ] {
        let decoded = decoder.decode_line(line);
        for chunk in &decoded.chunks {
            assert!(renderer.render(chunk).is_empty());
        }
    }
    assert!(!renderer.started());

    let decoded = decoder.decode_line(r#"{"type":"finish-step","usage":{"inputTokens":5,"outputTokens":0}}"#);
    for chunk in &decoded.chunks {
        assert!(
            renderer.render(chunk).is_empty(),
            "usage alone must not commit the response"
        );
    }
    assert!(!renderer.started());
    assert!(decoder.usage().completion_tokens == 0, "the guard reads this");
}
