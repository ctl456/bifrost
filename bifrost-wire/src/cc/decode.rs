//! Decoding the `cc/1.53.1` NDJSON stream into canonical chunks.
//!
//! The upstream answers a generation request with newline-delimited JSON, one
//! event per line, and emits event types well beyond the ones this build acts
//! on. Unknown types are reported, never fatal: the upstream adds event types on
//! its own schedule, and dropping a live stream over one would be worse than
//! ignoring it.

use bifrost_core::{Error, FinishReason, OutputChunk, Usage};
use serde_json::Value;

/// What one upstream line produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DecodedLine {
    pub chunks: Vec<OutputChunk>,
    /// Event type this build does not know how to interpret.
    pub unknown: Option<String>,
    /// Line that was not valid JSON, truncated for logging.
    pub malformed: Option<String>,
}

impl DecodedLine {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty() && self.unknown.is_none() && self.malformed.is_none()
    }
}

/// Decodes `cc` NDJSON lines.
#[derive(Debug, Default, Clone)]
pub struct CcDecoder {
    /// Finish reason reported when a step ended, used when the terminal event
    /// omits one.
    step_finish_reason: Option<FinishReason>,
    usage: Usage,
    last_event: Option<String>,
}

impl CcDecoder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The most recent accounting the upstream reported.
    ///
    /// This is the value the empty-response guard tests, which is not always the
    /// value the terminal frame carries: the upstream reports accounting once
    /// per step and again at the end, and only the guard reads the first one.
    #[must_use]
    pub const fn usage(&self) -> Usage {
        self.usage
    }

    /// The last event type seen, for diagnostics.
    #[must_use]
    pub fn last_event(&self) -> Option<&str> {
        self.last_event.as_deref()
    }

    /// Decode one line. Blank lines, SSE comments and `[DONE]` produce nothing.
    pub fn decode_line(&mut self, line: &str) -> DecodedLine {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" || trimmed.starts_with(':') {
            return DecodedLine::default();
        }

        let Ok(event) = serde_json::from_str::<Value>(trimmed) else {
            return DecodedLine {
                malformed: Some(truncate(trimmed, 200)),
                ..DecodedLine::default()
            };
        };
        let Some(kind) = event.get("type").and_then(Value::as_str) else {
            return DecodedLine::default();
        };
        self.last_event = Some(kind.to_owned());

        match kind {
            "text-delta" => self.text_delta(&event),
            "reasoning-delta" => self.reasoning_delta(&event),
            "tool-call" => self.tool_call(&event),
            "finish-step" => self.finish_step(&event),
            "finish" => self.finish(&event),
            "error" => self.upstream_error(&event),
            // Recognized but user-invisible: block boundaries, provider
            // metadata and the incremental tool-input stream that `tool-call`
            // already carries in full.
            "text-start" | "text-end" | "reasoning-start" | "reasoning-end" | "start" | "start-step"
            | "provider-metadata" | "tool-input-start" | "tool-input-delta" | "tool-input-end" | "tool-error" => {
                DecodedLine::default()
            }
            other => DecodedLine {
                unknown: Some(other.to_owned()),
                ..DecodedLine::default()
            },
        }
    }

    fn text_delta(&self, event: &Value) -> DecodedLine {
        // `text` is the documented field; `delta` is the older spelling.
        let text = event
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| event.get("delta").and_then(Value::as_str))
            .unwrap_or_default();
        if text.is_empty() {
            return DecodedLine::default();
        }
        DecodedLine {
            chunks: vec![OutputChunk::Text { text: text.to_owned() }],
            ..DecodedLine::default()
        }
    }

    fn reasoning_delta(&self, event: &Value) -> DecodedLine {
        let text = event.get("text").and_then(Value::as_str).unwrap_or_default();
        if text.is_empty() {
            return DecodedLine::default();
        }
        DecodedLine {
            chunks: vec![OutputChunk::Reasoning { text: text.to_owned() }],
            ..DecodedLine::default()
        }
    }

    fn tool_call(&self, event: &Value) -> DecodedLine {
        let id = event
            .get("toolCallId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned);
        let name = event
            .get("toolName")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        DecodedLine {
            chunks: vec![OutputChunk::ToolCall {
                id,
                name,
                arguments: argument_string(event.get("input")),
            }],
            ..DecodedLine::default()
        }
    }

    fn finish_step(&mut self, event: &Value) -> DecodedLine {
        if let Some(raw) = event.get("finishReason").and_then(Value::as_str) {
            self.step_finish_reason = Some(finish_reason(raw));
        }
        let mut chunks = Vec::new();
        if let Some(usage) = event.get("usage") {
            let usage = usage_from(usage);
            self.usage = usage;
            chunks.push(OutputChunk::Usage(usage));
        }
        DecodedLine {
            chunks,
            ..DecodedLine::default()
        }
    }

    fn finish(&mut self, event: &Value) -> DecodedLine {
        let reason = self.step_finish_reason.unwrap_or_else(|| {
            event
                .get("finishReason")
                .and_then(Value::as_str)
                .map_or(FinishReason::Stop, finish_reason)
        });
        // The terminal event usually repeats the totals; when it does not, the
        // per-step reading stands.
        let usage = event.get("totalUsage").map_or(self.usage, usage_from);
        self.usage = usage;
        DecodedLine {
            chunks: vec![OutputChunk::Usage(usage), OutputChunk::Finish { reason }],
            ..DecodedLine::default()
        }
    }

    fn upstream_error(&self, event: &Value) -> DecodedLine {
        let message = event
            .pointer("/error/message")
            .and_then(Value::as_str)
            .or_else(|| event.get("message").and_then(Value::as_str))
            .unwrap_or("Unknown CC error");
        let code = event
            .pointer("/error/code")
            .and_then(Value::as_str)
            .or_else(|| event.get("code").and_then(Value::as_str))
            .map(str::to_owned);
        DecodedLine {
            chunks: vec![OutputChunk::UpstreamError {
                error: Error::from_upstream(error_status(message), code, message),
            }],
            ..DecodedLine::default()
        }
    }
}

/// The HTTP status an in-stream error claims, read from a leading `<NNN>`.
///
/// The upstream has no status field mid-stream, so it prefixes the status onto
/// the message. A message without one is treated as a gateway failure, which is
/// what the original proxy assumed.
fn error_status(message: &str) -> u16 {
    let rest = message.strip_prefix('<').unwrap_or_default();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() == 3 {
        return digits.parse().unwrap_or(502);
    }
    502
}

/// Map an upstream finish reason onto the canonical enum.
///
/// An unrecognized reason degrades to `stop`: every adapter renders this value,
/// and passing an unknown string through would mean each client sees a reason
/// no SDK documents.
fn finish_reason(raw: &str) -> FinishReason {
    match raw {
        "tool-calls" | "tool_calls" => FinishReason::ToolCalls,
        "length" => FinishReason::Length,
        "content-filter" | "content_filter" => FinishReason::ContentFilter,
        "error" => FinishReason::Error,
        _ => FinishReason::Stop,
    }
}

/// The argument string for a tool call.
///
/// Kept verbatim when the upstream already sent a string, because re-encoding it
/// would renumber floats and re-order keys on bytes the client forwards to a
/// tool.
fn argument_string(input: Option<&Value>) -> String {
    match input {
        None | Some(Value::Null) => "{}".to_owned(),
        Some(Value::String(raw)) => raw.clone(),
        Some(value) => value.to_string(),
    }
}

/// Read an accounting record from the upstream's camelCase spelling.
///
/// Every input counter is zeroed when the upstream reports no output, matching
/// the original proxy's anti-false-billing rule. Bifrost zeroes the cache
/// counters too, not just the two the original had: leaving a later field out of
/// the rule would reopen the hole it exists to close.
fn usage_from(value: &Value) -> Usage {
    let details = value.get("inputTokenDetails");
    let detail = |key: &str| token_count(details.and_then(|details| details.get(key)));

    let mut usage = Usage {
        prompt_tokens: token_count(value.get("inputTokens")).unwrap_or(0),
        completion_tokens: token_count(value.get("outputTokens")).unwrap_or(0),
        reasoning_tokens: token_count(value.get("reasoningTokens")).unwrap_or(0),
        cached_tokens: token_count(value.get("cachedInputTokens"))
            .or_else(|| detail("cacheReadTokens"))
            .unwrap_or(0),
        cache_write_tokens: detail("cacheWriteTokens").unwrap_or(0),
        no_cache_tokens: detail("noCacheTokens"),
    };
    if usage.completion_tokens == 0 {
        usage.prompt_tokens = 0;
        usage.cached_tokens = 0;
        usage.cache_write_tokens = 0;
        usage.no_cache_tokens = None;
    }
    usage
}

/// Read a token count, tolerating a fractional or out-of-range number.
fn token_count(value: Option<&Value>) -> Option<u32> {
    let value = value?;
    if let Some(count) = value.as_u64() {
        return Some(count.min(u64::from(u32::MAX)) as u32);
    }
    if let Some(count) = value.as_f64()
        && count.is_finite()
        && count >= 0.0
    {
        return Some(count.min(f64::from(u32::MAX)) as u32);
    }
    None
}

fn truncate(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decode(lines: &[&str]) -> Vec<OutputChunk> {
        let mut decoder = CcDecoder::new();
        let mut chunks = Vec::new();
        for line in lines {
            chunks.extend(decoder.decode_line(line).chunks);
        }
        chunks
    }

    #[test]
    fn blank_comment_and_done_lines_produce_nothing() {
        let mut decoder = CcDecoder::new();
        for line in ["", "   ", ": keepalive", "[DONE]"] {
            assert!(decoder.decode_line(line).is_empty(), "{line:?} should be inert");
        }
        assert_eq!(decoder.last_event(), None);
    }

    #[test]
    fn text_deltas_accept_both_field_spellings() {
        assert_eq!(
            decode(&[r#"{"type":"text-delta","text":"hi"}"#]),
            vec![OutputChunk::Text { text: "hi".to_owned() }]
        );
        assert_eq!(
            decode(&[r#"{"type":"text-delta","delta":"old"}"#]),
            vec![OutputChunk::Text { text: "old".to_owned() }]
        );
        assert!(decode(&[r#"{"type":"text-delta","text":""}"#]).is_empty());
    }

    #[test]
    fn tool_calls_keep_the_argument_string_verbatim() {
        assert_eq!(
            decode(&[r#"{"type":"tool-call","toolCallId":"c1","toolName":"read","input":"{\"a\": 1}"}"#]),
            vec![OutputChunk::ToolCall {
                id: Some("c1".to_owned()),
                name: "read".to_owned(),
                arguments: r#"{"a": 1}"#.to_owned(),
            }]
        );
        assert_eq!(
            decode(&[r#"{"type":"tool-call","toolName":"read","input":{"b":2}}"#]),
            vec![OutputChunk::ToolCall {
                id: None,
                name: "read".to_owned(),
                arguments: r#"{"b":2}"#.to_owned(),
            }]
        );
        assert_eq!(
            decode(&[r#"{"type":"tool-call","toolName":"read","input":null}"#]),
            vec![OutputChunk::ToolCall {
                id: None,
                name: "read".to_owned(),
                arguments: "{}".to_owned(),
            }]
        );
    }

    #[test]
    fn an_empty_tool_call_id_becomes_absent() {
        assert_eq!(
            decode(&[r#"{"type":"tool-call","toolCallId":"","toolName":"read","input":{}}"#]),
            vec![OutputChunk::ToolCall {
                id: None,
                name: "read".to_owned(),
                arguments: "{}".to_owned(),
            }]
        );
    }

    #[test]
    fn finish_step_remembers_the_reason_for_the_terminal_event() {
        let chunks = decode(&[
            r#"{"type":"finish-step","finishReason":"tool-calls","usage":{"inputTokens":10,"outputTokens":4}}"#,
            r#"{"type":"finish"}"#,
        ]);
        assert_eq!(
            chunks,
            vec![
                OutputChunk::Usage(Usage {
                    prompt_tokens: 10,
                    completion_tokens: 4,
                    ..Usage::default()
                }),
                OutputChunk::Usage(Usage {
                    prompt_tokens: 10,
                    completion_tokens: 4,
                    ..Usage::default()
                }),
                OutputChunk::Finish {
                    reason: FinishReason::ToolCalls
                },
            ]
        );
    }

    #[test]
    fn the_terminal_event_overrides_per_step_usage() {
        let chunks = decode(&[
            r#"{"type":"finish-step","usage":{"inputTokens":10,"outputTokens":4}}"#,
            r#"{"type":"finish","totalUsage":{"inputTokens":99,"outputTokens":40,"cachedInputTokens":9}}"#,
        ]);
        assert_eq!(
            chunks.last(),
            Some(&OutputChunk::Finish {
                reason: FinishReason::Stop
            })
        );
        assert_eq!(
            chunks[chunks.len() - 2],
            OutputChunk::Usage(Usage {
                prompt_tokens: 99,
                completion_tokens: 40,
                cached_tokens: 9,
                ..Usage::default()
            })
        );
    }

    #[test]
    fn unknown_finish_reasons_degrade_to_stop() {
        assert_eq!(
            decode(&[r#"{"type":"finish","finishReason":"something-new"}"#]).last(),
            Some(&OutputChunk::Finish {
                reason: FinishReason::Stop
            })
        );
    }

    #[test]
    fn zero_output_zeroes_every_input_counter() {
        let chunks = decode(&[
            r#"{"type":"finish","totalUsage":{"inputTokens":7653,"outputTokens":0,"cachedInputTokens":7568,"inputTokenDetails":{"noCacheTokens":85,"cacheWriteTokens":7}}}"#,
        ]);
        assert_eq!(
            chunks[0],
            OutputChunk::Usage(Usage::default()),
            "a zero-output completion must not report billable input"
        );
    }

    #[test]
    fn usage_reads_the_uncached_count_and_cache_counters() {
        let mut decoder = CcDecoder::new();
        decoder.decode_line(
            r#"{"type":"finish","totalUsage":{"inputTokens":7653,"outputTokens":12,"cachedInputTokens":7568,"inputTokenDetails":{"noCacheTokens":85,"cacheReadTokens":7568,"cacheWriteTokens":11}}}"#,
        );
        let usage = decoder.usage();
        assert_eq!(usage.prompt_tokens, 7_653);
        assert_eq!(usage.completion_tokens, 12);
        assert_eq!(usage.cached_tokens, 7_568);
        assert_eq!(usage.cache_write_tokens, 11);
        assert_eq!(usage.non_cached_input_tokens(), 85);
    }

    #[test]
    fn a_missing_cache_read_counter_falls_back_to_the_detail_field() {
        let chunks = decode(&[
            r#"{"type":"finish","totalUsage":{"inputTokens":100,"outputTokens":3,"inputTokenDetails":{"cacheReadTokens":40}}}"#,
        ]);
        let Some(OutputChunk::Usage(usage)) = chunks.first() else {
            panic!("expected usage");
        };
        assert_eq!(usage.cached_tokens, 40);
        assert_eq!(usage.non_cached_input_tokens(), 60);
    }

    #[test]
    fn an_upstream_error_carries_its_prefixed_status() {
        let chunks =
            decode(&[r#"{"type":"error","error":{"message":"<402> quota exhausted","code":"USAGE_EXCEEDED"}}"#]);
        let Some(OutputChunk::UpstreamError { error }) = chunks.first() else {
            panic!("expected an error chunk");
        };
        assert_eq!(error.status, 429, "402 maps onto a retryable throttle");
        assert_eq!(error.retry_after, Some(30));
        assert_eq!(error.upstream_code.as_deref(), Some("USAGE_EXCEEDED"));
        assert_eq!(error.message, "<402> quota exhausted");
    }

    #[test]
    fn an_error_without_a_status_is_a_gateway_failure() {
        let chunks = decode(&[r#"{"type":"error","message":"connector exploded"}"#]);
        let Some(OutputChunk::UpstreamError { error }) = chunks.first() else {
            panic!("expected an error chunk");
        };
        assert_eq!(error.status, 502);
        assert_eq!(error.retry_after, None);
    }

    #[test]
    fn unknown_events_are_reported_not_fatal() {
        let mut decoder = CcDecoder::new();
        let decoded = decoder.decode_line(r#"{"type":"brand-new-event","payload":1}"#);
        assert!(decoded.chunks.is_empty());
        assert_eq!(decoded.unknown.as_deref(), Some("brand-new-event"));
        assert_eq!(decoder.last_event(), Some("brand-new-event"));
    }

    #[test]
    fn malformed_lines_are_reported_not_fatal() {
        let mut decoder = CcDecoder::new();
        let decoded = decoder.decode_line("{not json");
        assert!(decoded.chunks.is_empty());
        assert_eq!(decoded.malformed.as_deref(), Some("{not json"));
        assert_eq!(decoder.last_event(), None);
    }

    #[test]
    fn an_event_without_a_type_is_ignored() {
        let mut decoder = CcDecoder::new();
        assert!(decoder.decode_line(r#"{"payload":1}"#).is_empty());
        assert_eq!(decoder.last_event(), None);
    }

    #[test]
    fn silent_event_types_are_recognized() {
        let mut decoder = CcDecoder::new();
        for kind in [
            "text-start",
            "text-end",
            "reasoning-start",
            "reasoning-end",
            "start",
            "start-step",
            "provider-metadata",
            "tool-input-start",
            "tool-input-delta",
            "tool-input-end",
            "tool-error",
        ] {
            let decoded = decoder.decode_line(&format!(r#"{{"type":"{kind}"}}"#));
            assert!(decoded.is_empty(), "{kind} should be silent");
            assert_eq!(decoder.last_event(), Some(kind));
        }
    }

    #[test]
    fn usage_tolerates_fractional_and_oversized_counts() {
        assert_eq!(token_count(Some(&json!(12.0))), Some(12));
        assert_eq!(token_count(Some(&json!(-1))), None);
        assert_eq!(token_count(Some(&json!(1e30))), Some(u32::MAX));
        assert_eq!(token_count(Some(&json!("nope"))), None);
        assert_eq!(token_count(None), None);
    }
}
