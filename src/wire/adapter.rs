//! The upstream dialect contract.

use crate::config::AdapterId;
use crate::core::{CanonicalRequest, ConversionError};
use crate::fingerprint::Fingerprint;

use crate::wire::context::WireContext;
use crate::wire::entropy::Entropy;
use crate::wire::request::WireRequest;

/// Encodes a canonical request into one upstream dialect.
///
/// A dialect is an implementation, not a branch: adding a version means adding
/// a type here and registering it, leaving existing behavior untouched.
pub trait WireAdapter: Send + Sync {
    /// Dialect family, e.g. `cc`.
    fn name(&self) -> &'static str;
    /// Dialect version this implementation speaks.
    fn version(&self) -> &'static str;

    /// `<name>/<version>`.
    fn id(&self) -> String {
        format!("{}/{}", self.name(), self.version())
    }

    /// The package the upstream's own client is published as, when it is
    /// published under a name worth watching.
    ///
    /// A dialect is a reading of a client, and the client moves on its own
    /// schedule. Naming the package here puts the thing to re-read next to the
    /// implementation that must be re-read, and it is what a drift check reads;
    /// nothing a request carries comes from it. A dialect with no published
    /// reference answers `None`, and there is nothing to watch.
    fn published_package(&self) -> Option<&'static str> {
        None
    }

    fn encode(&self, request: &CanonicalRequest, context: &WireContext) -> Result<WireRequest, ConversionError>;

    /// The requests that announce a key to the upstream, before it generates.
    ///
    /// The device a key is using, and the fact that a session exists, are told to
    /// the upstream once in a while rather than on every request — the upstream
    /// records them, and repeating them per request would be noise. What that
    /// takes is part of speaking the dialect, so the dialect says; a dialect with
    /// nothing to announce returns nothing, which is the default.
    ///
    /// `fingerprint` is absent when the deployment does not report a device
    /// identity. The dialect decides what its remaining announcements look like
    /// without one; nothing here is required for a request to be served.
    fn announce(
        &self,
        context: &WireContext,
        fingerprint: Option<&Fingerprint>,
        entropy: &dyn Entropy,
    ) -> Vec<WireRequest> {
        let _ = (context, fingerprint, entropy);
        Vec::new()
    }

    /// The request that asks which models this account may use, if the upstream
    /// publishes a list of its own.
    ///
    /// Returned as a request rather than as a list because the answer is cached and
    /// read far more often than it is fetched, and because what is cached is the
    /// dialect's business rather than the edge's. `None` means there is no
    /// catalogue to ask for, and the table compiled into this build is the answer.
    ///
    /// The key is all this gets: a catalogue is about an account, not about a
    /// session, a directory or a turn.
    fn provider_models(&self, api_key: &str) -> Option<WireRequest> {
        let _ = api_key;
        None
    }
}

/// Every dialect this build implements.
pub const SUPPORTED: &[&str] = &["cc/1.53.1"];

/// Resolve a configured adapter id to an implementation.
///
/// Returns `None` for an unknown dialect so the caller can refuse to start
/// rather than silently falling back to a version it does not implement.
#[must_use]
pub fn adapter_for(id: &AdapterId) -> Option<Box<dyn WireAdapter>> {
    match (id.name.as_str(), id.version.as_str()) {
        ("cc", "1.53.1") => Some(Box::new(crate::wire::cc::CcV1531::default())),
        _ => None,
    }
}
