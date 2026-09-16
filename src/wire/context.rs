//! Per-request facts the envelope and headers are built from.
//!
//! Everything here comes from either the caller's configuration or a
//! mechanism's decision, never by inspecting the host.

use crate::fingerprint::DeviceProfile;

use crate::wire::entropy::Entropy;
use crate::wire::util::{Traceparent, is_uuid, slugify_path};

/// What the adapter needs to know about the request it is encoding.
#[derive(Debug, Clone)]
pub struct WireContext {
    pub api_key: String,
    /// Session id; only reported as `threadId` when it is a valid UUID.
    pub session_id: String,
    /// Derived from [`DeviceProfile::project_dir`], so the two cannot disagree.
    pub project_slug: String,
    pub traceparent: Traceparent,
    /// `YYYY-MM-DD`.
    pub today: String,
    pub device: DeviceProfile,
    pub mode: String,
    pub permission_mode: String,
    pub zdr: bool,
}

impl WireContext {
    #[must_use]
    pub fn new(
        api_key: impl Into<String>,
        session_id: impl Into<String>,
        device: DeviceProfile,
        today: impl Into<String>,
        entropy: &dyn Entropy,
    ) -> Self {
        let project_slug = slugify_path(&device.project_dir);
        Self {
            api_key: api_key.into(),
            session_id: session_id.into(),
            project_slug,
            traceparent: Traceparent::generate(entropy),
            today: today.into(),
            device,
            mode: "agent".to_owned(),
            permission_mode: "standard".to_owned(),
            zdr: false,
        }
    }

    #[must_use]
    pub fn with_mode(mut self, mode: impl Into<String>) -> Self {
        self.mode = mode.into();
        self
    }

    #[must_use]
    pub fn with_zdr(mut self, zdr: bool) -> Self {
        self.zdr = zdr;
        self
    }

    /// The session id, but only when it is shaped like a UUID.
    ///
    /// The upstream rejects a malformed `threadId` by dropping the whole key, so
    /// the adapter omits it rather than sending something invalid.
    #[must_use]
    pub fn thread_id(&self) -> Option<&str> {
        is_uuid(&self.session_id).then_some(self.session_id.as_str())
    }
}
