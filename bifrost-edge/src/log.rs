//! Where this process says what it is doing.
//!
//! One place, because the deployment configures one thing: how much is said and
//! in what shape. Everything the edge reports goes through here, so a level is a
//! filter rather than a decoration, and a message that does not clear it costs
//! nothing.

use std::sync::OnceLock;

use bifrost_config::{LogFormat, LogLevel};
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// What the deployment asked for.
#[derive(Debug, Clone, Copy)]
struct Settings {
    level: LogLevel,
    format: LogFormat,
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// Point the log at the deployment's settings.
///
/// Called once, by the process. Until it is called the defaults apply, because
/// the messages that arrive before it are the ones about failing to load the
/// configuration at all: they have to be able to get out.
pub fn init(level: LogLevel, format: LogFormat) {
    let _ = SETTINGS.set(Settings { level, format });
}

/// Something the deployment should look at.
pub fn warn(message: impl Into<String>) {
    emit(LogLevel::Warn, message.into());
}

/// Something that happened and is worth knowing.
pub fn info(message: impl Into<String>) {
    emit(LogLevel::Info, message.into());
}

/// Something that stopped a turn, or the process.
pub fn error(message: impl Into<String>) {
    emit(LogLevel::Error, message.into());
}

/// One line per request, whatever it answered.
///
/// Written by the middleware rather than by the endpoint, because the requests an
/// operator goes looking for are the ones that never reached an endpoint: a 404, a
/// preflight, a body refused before it was read. A line written inside a handler
/// would not exist for any of them.
///
/// It is written when the answer begins rather than when it ends, so a stream that
/// runs for minutes is a line about the wait its client felt. The end of a stream,
/// and what it cost, is what the evidence archive is for.
///
/// [`Detail`] carries what the turn learned about itself while it answered, and
/// how much it learned depends on how far it got — a line with no model is a turn
/// that never decoded a body, which is information rather than a gap.
pub fn access(entry: &Access) {
    let Some(settings) = shape_for(LogLevel::Info) else {
        return;
    };
    write_line(render_access(entry, settings.format, &now()));
}

/// What one request was and what it answered.
///
/// The request line is the one thing about a turn that is true whether it
/// succeeded, failed or was refused, so it is always present.
#[derive(Debug, Clone)]
pub struct Access {
    pub method: String,
    pub path: String,
    pub status: u16,
    /// How long the answer took to begin: the whole turn for a non-streaming
    /// request, and time to the first frame for a stream — which is the number a
    /// client is actually waiting on.
    pub ms: u64,
    pub detail: Detail,
}

/// What a turn learned about itself.
///
/// Filled in as the turn goes — the protocol before the body is read, the model
/// once it decodes, the key once it authenticates — so a request refused early
/// leaves the fields it never reached absent instead of guessing at them.
///
/// The key appears as its fingerprint and never as itself: the log is read by more
/// people than the key was given to, which is the rule the journal follows too.
#[derive(Debug, Clone, Default)]
pub struct Detail {
    pub protocol: Option<String>,
    pub model: Option<String>,
    pub stream: Option<bool>,
    pub key_fingerprint: Option<String>,
}

/// One line: the request, then the pairs the turn knew, in a fixed order.
///
/// The pairs are in the message for `text` and in fields of their own for `json`,
/// so neither reader has to parse the other's half. An absent field is written as
/// null rather than left out, so a reader can tell a turn that had no model from a
/// writer that stopped recording them.
fn render_access(entry: &Access, format: LogFormat, at: &str) -> String {
    let mut message = format!("{} {} {} {}ms", entry.method, entry.path, entry.status, entry.ms);
    for (name, value) in access_pairs(&entry.detail) {
        message.push_str(&format!(" {name}={value}"));
    }
    match format {
        LogFormat::Text => format!("{at} {:<5} {message}", LogLevel::Info.name()),
        LogFormat::Json => json!({
            "time": at,
            "level": LogLevel::Info.name(),
            "msg": message,
            "method": entry.method,
            "path": entry.path,
            "status": entry.status,
            "ms": entry.ms,
            "protocol": entry.detail.protocol,
            "model": entry.detail.model,
            "stream": entry.detail.stream,
            "key": entry.detail.key_fingerprint,
        })
        .to_string(),
    }
}

fn access_pairs(detail: &Detail) -> Vec<(&'static str, String)> {
    let mut pairs = Vec::new();
    if let Some(protocol) = &detail.protocol {
        pairs.push(("protocol", protocol.clone()));
    }
    if let Some(model) = &detail.model {
        pairs.push(("model", model.clone()));
    }
    if let Some(stream) = detail.stream {
        pairs.push(("stream", stream.to_string()));
    }
    if let Some(key) = &detail.key_fingerprint {
        pairs.push(("key", key.clone()));
    }
    pairs
}

/// Whether a message at `level` clears what the deployment asked for.
///
/// The ordering of [`LogLevel`] is what makes this a comparison: a deployment
/// that asked for `info` gets errors, warnings and information, and not debug.
fn clears(level: LogLevel, configured: LogLevel) -> bool {
    level <= configured
}

fn settings() -> Settings {
    SETTINGS.get().copied().unwrap_or(Settings {
        level: LogLevel::Info,
        format: LogFormat::Text,
    })
}

fn emit(level: LogLevel, message: String) {
    let Some(settings) = shape_for(level) else {
        return;
    };
    write_line(render(level, &message, settings.format, &now()));
}

/// Whether a line at `level` is written at all, and in what shape.
///
/// Used by the two things that write lines — a message and a request — so that a
/// level is a filter in one place rather than a condition repeated at each call
/// site.
fn shape_for(level: LogLevel) -> Option<Settings> {
    let settings = settings();
    clears(level, settings.level).then_some(settings)
}

/// Put a rendered line where the deployment asked for it.
///
/// stderr rather than stdout: the process says what it is doing, and a deployment
/// that captures the two separately should not find them mixed into a
/// redirectable stream.
fn write_line(line: String) {
    eprintln!("{line}");
}

/// One line, in the shape the deployment asked for.
fn render(level: LogLevel, message: &str, format: LogFormat, at: &str) -> String {
    match format {
        LogFormat::Text => format!("{at} {:<5} {message}", level.name()),
        LogFormat::Json => json!({
            "time": at,
            "level": level.name(),
            "msg": message,
        })
        .to_string(),
    }
}

/// The current instant, to the second, in UTC.
fn now() -> String {
    OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_carries_the_time_the_level_and_the_message() {
        let line = render(
            LogLevel::Warn,
            "the catalogue did not answer",
            LogFormat::Text,
            "2026-09-15T12:00:00Z",
        );
        assert_eq!(line, "2026-09-15T12:00:00Z warn  the catalogue did not answer");
    }

    /// A line per object is the point of the shape: a collector reads one object
    /// per line, and a message with a newline in it would break that.
    #[test]
    fn json_is_one_object() {
        let line = render(LogLevel::Info, "listening", LogFormat::Json, "2026-09-15T12:00:00Z");
        assert_eq!(
            line,
            r#"{"time":"2026-09-15T12:00:00Z","level":"info","msg":"listening"}"#
        );
    }

    /// The line a person reads: the request first, then what the turn knew.
    #[test]
    fn a_request_is_one_line_with_its_detail() {
        assert_eq!(
            render_access(&worked(), LogFormat::Text, "2026-09-15T12:00:00Z"),
            "2026-09-15T12:00:00Z info  POST /v1/chat/completions 200 1234ms \
protocol=openai-chat model=deepseek/deepseek-v4-flash stream=true key=0123456789abcdef"
        );
    }

    /// The line a collector reads: the same fields as fields, so that finding the
    /// model does not mean parsing the message.
    #[test]
    fn a_json_request_keeps_its_detail_apart_from_the_message() {
        assert_eq!(
            render_access(&worked(), LogFormat::Json, "2026-09-15T12:00:00Z"),
            concat!(
                r#"{"time":"2026-09-15T12:00:00Z","level":"info","#,
                r#""msg":"POST /v1/chat/completions 200 1234ms protocol=openai-chat "#,
                r#"model=deepseek/deepseek-v4-flash stream=true key=0123456789abcdef","#,
                r#""method":"POST","path":"/v1/chat/completions","status":200,"ms":1234,"#,
                r#""protocol":"openai-chat","model":"deepseek/deepseek-v4-flash","#,
                r#""stream":true,"key":"0123456789abcdef"}"#
            )
        );
    }

    /// A request that never got as far as a body still gets a line: the fields it
    /// did not reach are null, which says how far it got.
    #[test]
    fn a_request_that_learned_nothing_still_reports_itself() {
        let entry = Access {
            method: "GET".to_owned(),
            path: "/health".to_owned(),
            status: 200,
            ms: 0,
            detail: Detail::default(),
        };
        assert_eq!(
            render_access(&entry, LogFormat::Text, "2026-09-15T12:00:00Z"),
            "2026-09-15T12:00:00Z info  GET /health 200 0ms"
        );
        assert_eq!(
            render_access(&entry, LogFormat::Json, "2026-09-15T12:00:00Z"),
            concat!(
                r#"{"time":"2026-09-15T12:00:00Z","level":"info","msg":"GET /health 200 0ms","#,
                r#""method":"GET","path":"/health","status":200,"ms":0,"#,
                r#""protocol":null,"model":null,"stream":null,"key":null}"#
            )
        );
    }

    /// The key is never in the line, only the name of it: a deployment that logged
    /// both would be shipping credentials to everyone who can read its logs.
    #[test]
    fn the_key_never_appears_as_itself() {
        let entry = worked();
        let line = render_access(&entry, LogFormat::Text, "2026-09-15T12:00:00Z");
        assert!(!line.contains("user_"));
        assert!(line.ends_with("key=0123456789abcdef"));
    }

    fn worked() -> Access {
        Access {
            method: "POST".to_owned(),
            path: "/v1/chat/completions".to_owned(),
            status: 200,
            ms: 1234,
            detail: Detail {
                protocol: Some("openai-chat".to_owned()),
                model: Some("deepseek/deepseek-v4-flash".to_owned()),
                stream: Some(true),
                key_fingerprint: Some("0123456789abcdef".to_owned()),
            },
        }
    }

    #[test]
    fn a_level_keeps_itself_and_everything_louder() {
        assert!(clears(LogLevel::Error, LogLevel::Info));
        assert!(clears(LogLevel::Warn, LogLevel::Info));
        assert!(clears(LogLevel::Info, LogLevel::Info));
        assert!(!clears(LogLevel::Debug, LogLevel::Info));
        assert!(clears(LogLevel::Debug, LogLevel::Debug));
        assert!(!clears(LogLevel::Info, LogLevel::Error), "asked for errors only");
    }
}
