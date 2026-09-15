//! Fidelity check for the outbound stream, against the original proxy.
//!
//! `fixtures/responses_sse_golden.json` is produced by executing
//! `createResponsesSseTranslator` from `commandcode-proxy/proxy.mjs` verbatim and
//! recording the frames it emits for a set of upstream event streams. This test
//! feeds the same lines through Bifrost's decoder and renderer and requires the
//! frames to be byte-identical — key order included, because the JSON *string* is
//! what the client parses.
//!
//! Two things the fixture normalizes, both recorded in its own header: the item
//! ids, which the original mints as UUIDs, and the clock behind `completed_at`.
//! Everything else is verbatim.
//!
//! One intentional departure, which no case in the fixture could carry because
//! the fixture is generated from the original and would disagree with it by
//! construction:
//!
//! - A truncated turn stays truncated when the terminal event omits its reason.
//!   The original's Responses translator has no `finish-step` arm, so a step that
//!   reported `length` and a terminal event that reported nothing produce
//!   `status: "completed"` — and `usage` describing a turn the client was in fact
//!   cut off from. The original does not make that mistake on its Chat path, where
//!   it falls back to the step reason. Bifrost keeps the step reason everywhere,
//!   which is why `a_truncation_omitted_by_the_terminal_event_stays_truncated`
//!   exists below.

use bifrost_protocol::openai::responses::ResponsesStreamRenderer;
use bifrost_wire::cc::CcDecoder;
use serde_json::Value;

static FIXTURE: std::sync::LazyLock<Value> = std::sync::LazyLock::new(|| {
    serde_json::from_str(include_str!("../fixtures/responses_sse_golden.json")).expect("golden fixture parses")
});

fn renderer() -> ResponsesStreamRenderer {
    ResponsesStreamRenderer::new(
        FIXTURE["model"].as_str().expect("model"),
        FIXTURE["id"].as_str().expect("id"),
        FIXTURE["created"].as_i64().expect("created"),
    )
}

fn as_lines(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("lines")
        .iter()
        .map(|line| line.as_str().expect("line").to_owned())
        .collect()
}

/// Drives one golden case and returns the frames as strings.
fn frames(lines: &[String]) -> (Vec<String>, CcDecoder, ResponsesStreamRenderer) {
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
    frames.extend(renderer.end_events().iter().map(|frame| frame.as_str().to_owned()));
    (frames, decoder, renderer)
}

fn as_strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("frames")
        .iter()
        .map(|frame| frame.as_str().expect("frame").to_owned())
        .collect()
}

#[test]
fn every_golden_stream_produces_identical_frames() {
    let cases = FIXTURE["cases"].as_array().expect("cases");
    assert!(cases.len() >= 5, "the fixture must stay representative");

    for case in cases {
        let name = case["name"].as_str().expect("case name");
        let expected = as_strings(&case["frames"]);
        let (actual, _, _) = frames(&as_lines(&case["lines"]));

        assert_eq!(
            actual, expected,
            "frames diverged for case `{name}`\n  proxy:   {expected:#?}\n  bifrost: {actual:#?}"
        );
    }
}

#[test]
fn a_quiet_stream_is_left_to_the_edge_to_answer() {
    // Nothing visible was produced, so nothing was written: the renderer must not
    // have committed the response, or the edge could no longer answer with a
    // retryable status.
    let (actual, _, renderer) = frames(&as_lines(&FIXTURE["silent_case"]["lines"]));
    assert_eq!(actual, as_strings(&FIXTURE["silent_case"]["frames"]));
    assert!(actual.is_empty(), "a zero-output stream emits no frames");
    assert!(!renderer.started(), "the response must stay uncommitted");
}

#[test]
fn a_failure_is_framed_with_the_classification_the_upstream_claimed() {
    let case = &FIXTURE["error_case"];
    let lines = as_lines(&case["lines"]);

    // What the client was sent before the upstream broke. The terminal event is
    // absent on purpose: the failure is framed instead, and a client left waiting
    // for `response.completed` would read a broken turn as a finished one.
    let (written, _, mut renderer) = frames(&lines);
    assert_eq!(written, as_strings(&case["frames"]), "the stream up to the failure");

    assert!(renderer.started(), "the text delta already committed the response");
    assert!(renderer.has_error(), "an upstream error must not look like a finish");

    // Each failure frame is read from its own renderer, because each one advances
    // the sequence counter the next would be numbered from.
    assert_eq!(
        renderer.error_frame("slow down").as_str(),
        case["error_frame"].as_str().expect("error_frame")
    );
    let (_, _, mut renderer) = frames(&lines);
    assert_eq!(
        renderer
            .failure_frames("slow down")
            .iter()
            .map(bifrost_protocol::SseFrame::as_str)
            .collect::<Vec<_>>(),
        as_strings(&case["failed_frames"])
    );
}

#[test]
fn a_truncation_omitted_by_the_terminal_event_stays_truncated() {
    // `finish-step` reports the reason, `finish` does not repeat it: the turn was
    // cut off by the output cap, and a client that is told `completed` will treat
    // the missing tail as the model's whole answer.
    let lines = as_lines(&serde_json::json!([
        r#"{"type":"text-delta","text":"half an ans"}"#,
        r#"{"type":"finish-step","finishReason":"length","usage":{"inputTokens":3,"outputTokens":1}}"#,
        r#"{"type":"finish","totalUsage":{"inputTokens":3,"outputTokens":1}}"#,
    ]));
    let (frames, _, _) = frames(&lines);
    let terminal = frames.last().expect("a terminal frame");

    assert!(terminal.starts_with("event: response.incomplete\n"), "{terminal}");
    let payload: Value = serde_json::from_str(
        terminal
            .lines()
            .nth(1)
            .and_then(|line| line.strip_prefix("data: "))
            .expect("the data line"),
    )
    .expect("the frame parses");
    assert_eq!(payload["response"]["status"], "incomplete", "{terminal}");
    assert_eq!(
        payload["response"]["incomplete_details"],
        serde_json::json!({ "reason": "max_output_tokens" }),
        "{terminal}"
    );
}

#[test]
fn item_ids_are_positional_not_random() {
    // The original mints a UUID per item, so its own output is unreproducible and
    // two runs of the same conversation differ. Bifrost numbers items by the index
    // they are reported at, which keeps a replay byte-identical; the fixture is
    // normalized to that form.
    let (actual, _, _) = frames(&as_lines(&FIXTURE["cases"][0]["lines"]));
    for frame in &actual {
        assert!(
            !frame.contains("msg_0-") && !frame.contains("rs_0-") && !frame.contains("fc_0-"),
            "an id kept a UUID shape: {frame}"
        );
    }
    assert!(actual[2].contains(r#""id":"msg_0""#), "{}", actual[2]);
}
