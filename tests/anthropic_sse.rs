//! Fidelity check for the outbound stream, against the original proxy.
//!
//! `fixtures/anthropic_sse_golden.json` is produced by executing
//! `createAnthropicSseTranslator` from `commandcode-proxy/proxy.mjs` verbatim and
//! recording the frames it emits for a set of upstream event streams. This test
//! feeds the same lines through Bifrost's decoder and renderer and requires the
//! events to match byte for byte — key order included, because the JSON *string*
//! is what the client parses.
//!
//! The original batches several events into one write and Bifrost returns them
//! separately, so the comparison is per event rather than per write: the bytes
//! and the event boundaries are both part of the contract, the batching is not.

use bifrost::core::OutputChunk;
use bifrost::protocol::anthropic::AnthropicStreamRenderer;
use bifrost::protocol::anthropic::response::error_frame;
use bifrost::wire::cc::CcDecoder;
use serde_json::Value;

static FIXTURE: std::sync::LazyLock<Value> = std::sync::LazyLock::new(|| {
    serde_json::from_str(include_str!("../fixtures/anthropic_sse_golden.json")).expect("golden fixture parses")
});

fn renderer() -> AnthropicStreamRenderer {
    AnthropicStreamRenderer::new(
        FIXTURE["model"].as_str().expect("model"),
        FIXTURE["id"].as_str().expect("id"),
    )
}

/// Split a stream into the events a client would parse out of it.
fn events(stream: &str) -> Vec<String> {
    stream
        .split("\n\n")
        .filter(|event| !event.is_empty())
        .map(|event| format!("{event}\n\n"))
        .collect()
}

/// Drive one upstream stream through the decoder and the renderer.
///
/// A failure is intercepted the way the edge intercepts it: the renderer is told
/// about it — so the stream is not closed as if it had ended — and the frame is
/// written by the caller, which is the only party that still knows whether a
/// status is possible.
fn translate(lines: &[&str]) -> (Vec<String>, CcDecoder) {
    let mut decoder = CcDecoder::new();
    let mut renderer = renderer();
    let mut stream = String::new();
    for line in lines {
        for chunk in decoder.decode_line(line).chunks {
            for frame in renderer.render(&chunk) {
                stream.push_str(frame.as_str());
            }
            if let OutputChunk::UpstreamError { error } = &chunk {
                stream.push_str(error_frame(error).as_str());
            }
        }
    }
    for frame in renderer.end_events() {
        stream.push_str(frame.as_str());
    }
    (events(&stream), decoder)
}

fn golden_lines(case: &Value) -> Vec<&str> {
    case["lines"]
        .as_array()
        .expect("lines")
        .iter()
        .map(|line| line.as_str().expect("line"))
        .collect()
}

fn golden_events(case: &Value) -> Vec<String> {
    events(
        &case["frames"]
            .as_array()
            .expect("frames")
            .iter()
            .map(|frame| frame.as_str().expect("frame"))
            .collect::<String>(),
    )
}

#[test]
fn every_golden_stream_produces_identical_events() {
    let cases = FIXTURE["cases"].as_array().expect("cases");
    assert!(cases.len() >= 5, "the fixture must stay representative");

    for case in cases {
        let name = case["name"].as_str().expect("case name");
        let (actual, decoder) = translate(&golden_lines(case));
        let expected = golden_events(case);
        assert_eq!(
            actual, expected,
            "events diverged for case `{name}`\n  proxy:   {expected:#?}\n  bifrost: {actual:#?}"
        );

        // The parser's own accounting is what the empty-output guard reads.
        assert_eq!(
            decoder.usage().completion_tokens as u64,
            case["output_tokens"].as_u64().expect("output_tokens"),
            "output tokens diverged for `{name}`"
        );
        assert_eq!(
            decoder.usage().prompt_tokens as u64,
            case["input_tokens"].as_u64().expect("input_tokens"),
            "input tokens diverged for `{name}`"
        );
    }
}

#[test]
fn a_failure_is_framed_with_the_classification_the_upstream_claimed() {
    let case = &FIXTURE["error_case"];
    let (actual, _) = translate(&golden_lines(case));
    let expected = golden_events(case);
    assert_eq!(actual, expected, "the error frame diverged");
    assert_eq!(
        error_frame(&decoder_error(case)).as_str(),
        case["error_frame"].as_str().expect("error_frame")
    );
}

/// The failure a golden case ends on.
fn decoder_error(case: &Value) -> bifrost::core::Error {
    let mut decoder = CcDecoder::new();
    for line in golden_lines(case) {
        for chunk in decoder.decode_line(line).chunks {
            if let OutputChunk::UpstreamError { error } = chunk {
                return error;
            }
        }
    }
    panic!("the case ends without a failure");
}

#[test]
fn a_quiet_stream_is_left_to_the_edge_to_answer() {
    let case = &FIXTURE["silent_case"];
    let (actual, _) = translate(&golden_lines(case));
    assert!(
        actual.is_empty(),
        "nothing was written, so the edge must still be able to answer with a status: {actual:#?}"
    );
    // What the original does instead: it opens the stream and then fails inside
    // it, which leaves a client to discover the failure from a 200 response.
    assert_eq!(
        golden_events(case).len(),
        2,
        "the golden case records the original's `message_start` plus the failure"
    );
}
