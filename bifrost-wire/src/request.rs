//! An encoded upstream request.

/// A fully-formed request, ready to be sent.
#[derive(Debug, Clone)]
pub struct WireRequest {
    pub method: &'static str,
    /// Path relative to the configured base URL.
    pub path: String,
    /// Header name/value pairs in the order the adapter emits them.
    pub headers: Vec<(String, String)>,
    /// The JSON to send, or `None` when this request carries no body at all.
    ///
    /// Absent rather than `null`: a lookup is a `GET`, and a serialized `null`
    /// under it is a body the upstream never asked for.
    pub body: Option<serde_json::Value>,
}

impl WireRequest {
    /// Case-insensitive header lookup.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Join the path onto a base URL.
    #[must_use]
    pub fn url(&self, api_base: &str) -> String {
        format!("{}{}", api_base.trim_end_matches('/'), self.path)
    }

    /// The body as it goes on the wire, or `None` when there is none.
    #[must_use]
    pub fn body_string(&self) -> Option<String> {
        self.body.as_ref().map(serde_json::Value::to_string)
    }
}
