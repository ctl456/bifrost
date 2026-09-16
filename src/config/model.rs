//! The configuration schema.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The only configuration schema version this build understands.
pub const CONFIG_VERSION: u32 = 1;

/// The effective configuration.
///
/// Every field has a default, so a partial file is valid. Unknown fields are a
/// hard error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub host: String,
    pub port: u16,
    pub api_base: String,
    pub device: DeviceConfig,
    pub wire: WireConfig,
    pub limits: LimitsConfig,
    pub mechanisms: MechanismsConfig,
    pub models: ModelsConfig,
    pub access: AccessConfig,
    pub fingerprint: FingerprintConfig,
    pub telemetry: TelemetryConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            host: "0.0.0.0".to_owned(),
            port: 3050,
            api_base: "https://api.commandcode.ai".to_owned(),
            device: DeviceConfig::default(),
            wire: WireConfig::default(),
            limits: LimitsConfig::default(),
            mechanisms: MechanismsConfig::default(),
            models: ModelsConfig::default(),
            access: AccessConfig::default(),
            fingerprint: FingerprintConfig::default(),
            telemetry: TelemetryConfig::default(),
        }
    }
}

/// The machine this deployment claims to be.
///
/// A profile rather than a set of knobs: the fingerprint, the request
/// environment, the working directory, the project slug and the lifecycle
/// metadata all come from here, so a deployment cannot describe itself two ways.
/// Only the working directory is configurable, because it is the one part of the
/// profile that has to look plausible to a person; the rest is the original's.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeviceConfig {
    /// The working directory to report, and what the project slug is derived
    /// from. Unset means the built-in profile, rather than a second copy of its
    /// path kept here to drift away from it.
    ///
    /// The original reads the same thing from `CC_DEVICE_PROJECT_DIR`.
    ///
    /// Not serialized when it is unset, so that a printed configuration is a TOML
    /// file that can be read back rather than one with a null in it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<String>,
}

/// An upstream protocol adapter identifier, e.g. `cc/1.53.1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterId {
    pub name: String,
    pub version: String,
}

/// Which upstream dialect to speak, and how to react when it drifts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WireConfig {
    /// `<name>/<major>.<minor>.<patch>`.
    pub adapter: String,
    /// Warn when the published client version moves ahead of the implemented one.
    ///
    /// A warning and nothing else. The version reported upstream is always the one
    /// this build implements: claiming a version whose shape is not implemented is
    /// a stronger signal than claiming an older one, so the published version is
    /// read to be compared against, never to be adopted.
    pub drift_watch: bool,
    /// Where the published version of the client is read from.
    ///
    /// A registry rather than an API host, and configurable because a mirror is
    /// the difference between a check that answers and one that times out.
    pub drift_registry: String,
    /// Ask the upstream to route only through zero-data-retention providers.
    pub zdr: bool,
}

impl Default for WireConfig {
    fn default() -> Self {
        Self {
            adapter: "cc/1.53.1".to_owned(),
            drift_watch: true,
            drift_registry: "https://registry.npmjs.org".to_owned(),
            zdr: false,
        }
    }
}

impl WireConfig {
    /// Parse [`WireConfig::adapter`], returning `None` when it is malformed.
    #[must_use]
    pub fn parse_adapter(&self) -> Option<AdapterId> {
        let (name, version) = self.adapter.split_once('/')?;
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return None;
        }
        let parts: Vec<&str> = version.split('.').collect();
        if parts.len() != 3 {
            return None;
        }
        if !parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
        {
            return None;
        }
        Some(AdapterId {
            name: name.to_owned(),
            version: version.to_owned(),
        })
    }
}

/// Resource ceilings and timeouts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    /// Request body ceiling. Oversized requests get `413` and the connection is
    /// drained so it stays reusable.
    pub max_body_mb: u32,
    /// Upstream read-idle timeout while streaming.
    pub stream_idle_ms: u64,
    /// Upstream read-idle timeout for non-streaming requests.
    pub nonstream_idle_ms: u64,
    /// In-flight request ceiling for the process; `0` means unlimited.
    pub max_inflight: u32,
    /// Watchdog for a downstream client that stopped reading; `0` disables it.
    pub client_stall_ms: u64,
    /// How long the pre-flight that announces a key to the upstream may take.
    ///
    /// The turn that triggers one waits for it, so that the upstream knows the
    /// device before it is asked to generate. A pre-flight that overruns is
    /// abandoned and the turn proceeds: the introduction is worth waiting a
    /// little for and never worth failing a request over.
    pub announce_ms: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_body_mb: 100,
            stream_idle_ms: 30_000,
            nonstream_idle_ms: 90_000,
            max_inflight: 0,
            client_stall_ms: 0,
            announce_ms: 10_000,
        }
    }
}

impl LimitsConfig {
    #[must_use]
    pub const fn max_body_bytes(&self) -> u64 {
        self.max_body_mb as u64 * 1024 * 1024
    }

    #[must_use]
    pub const fn stream_idle(&self) -> Duration {
        Duration::from_millis(self.stream_idle_ms)
    }

    #[must_use]
    pub const fn nonstream_idle(&self) -> Duration {
        Duration::from_millis(self.nonstream_idle_ms)
    }

    /// How long the announcement pre-flight may take before it is abandoned.
    #[must_use]
    pub const fn announce(&self) -> Duration {
        Duration::from_millis(self.announce_ms)
    }

    /// How long a client may stop taking bytes before the upstream is cut loose,
    /// or `None` when the watchdog is off.
    #[must_use]
    pub const fn client_stall(&self) -> Option<Duration> {
        if self.client_stall_ms == 0 {
            None
        } else {
            Some(Duration::from_millis(self.client_stall_ms))
        }
    }

    /// The idle timeout to apply for a given request shape.
    #[must_use]
    pub const fn idle_for(&self, streaming: bool) -> Duration {
        if streaming {
            self.stream_idle()
        } else {
            self.nonstream_idle()
        }
    }
}

/// Optional behaviors.
///
/// The three that reproduce the original proxy's behavior default to on; every
/// mechanism that adds new work is off until an operator asks for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MechanismsConfig {
    /// Report the derived device identity to the upstream.
    ///
    /// The identity is a deterministic function of the key either way; this is
    /// whether it is recorded upstream, which is the only thing that makes it
    /// observable.
    pub fingerprint: bool,
    /// Announce a key to the upstream before it is used to generate.
    ///
    /// The pre-flight that carries the fingerprint record and the session event,
    /// once per key per window. Turning this off stops both.
    pub lifecycle: bool,
    /// Per-key session id with expiry and jitter.
    pub session: bool,
    /// Forward prompt-cache breakpoints.
    pub prompt_cache: bool,
    /// Hint the upstream to compact context after repeated timeouts.
    pub timeout_context_hint: bool,
}

impl Default for MechanismsConfig {
    fn default() -> Self {
        Self {
            fingerprint: true,
            lifecycle: true,
            session: true,
            prompt_cache: false,
            timeout_context_hint: false,
        }
    }
}

/// What `/v1/models` answers with, and where that answer comes from.
///
/// The endpoint is a compatibility surface: a client asks what it may request
/// before it requests it. The list can come from the upstream, which knows what it
/// offers, or from the table compiled into this build, which is what a deployment
/// that cannot reach the catalogue falls back to.
///
/// It is a list of what the provider serves rather than of what this account may
/// use: a model a plan does not include is listed and still answers
/// `401 MODEL_NOT_IN_PLAN`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelsConfig {
    /// Ask the upstream what it offers.
    ///
    /// Off means the built-in table is the whole answer, and no call is made. The
    /// answer is not filtered to this account's plan, because the upstream does not
    /// filter it either.
    pub provider: bool,
    /// How long a fetched list is reused before it is fetched again.
    ///
    /// `0` fetches on every request, which is only sensible against an upstream
    /// whose catalogue changes faster than its clients start.
    pub refresh_ms: u64,
    /// How long a fetch may take before the built-in table is used instead.
    pub timeout_ms: u64,
    /// Rules that point a client's model name at one this account may use.
    ///
    /// A name no rule matches is forwarded as the client spelled it, refusal and
    /// all: the upstream's answer about a model it does not serve is the true
    /// answer, and a silent substitution would turn a typo into an answer from
    /// another model.
    ///
    /// Not serialized when empty, so a printed configuration reads back as the
    /// configuration it was printed from instead of gaining an empty table.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub aliases: BTreeMap<String, String>,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            provider: true,
            refresh_ms: 5 * 60 * 1000,
            timeout_ms: 10_000,
            aliases: BTreeMap::new(),
        }
    }
}

impl ModelsConfig {
    #[must_use]
    pub const fn refresh(&self) -> Duration {
        Duration::from_millis(self.refresh_ms)
    }

    #[must_use]
    pub const fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }

    /// The model to send in place of `requested`, when a rule covers that name.
    ///
    /// A rule covers a name when the name begins with the rule's pattern, ASCII
    /// case-insensitively, and the longest matching pattern wins: with `claude-`
    /// and `claude-haiku-` both configured, `claude-haiku-4-5` resolves through
    /// the second, which is the rule that named it. An exact pattern is the
    /// longest pattern that can match its own name, so it needs no special case.
    ///
    /// Nothing matched means nothing to change, rather than a default: this is
    /// the whole mechanism, and it invents no names.
    #[must_use]
    pub fn resolve(&self, requested: &str) -> Option<&str> {
        let lower = requested.to_ascii_lowercase();
        self.aliases
            .iter()
            .filter(|(pattern, _)| !pattern.is_empty() && lower.starts_with(&pattern.to_ascii_lowercase()))
            .max_by_key(|(pattern, _)| pattern.len())
            .map(|(_, model)| model.as_str())
    }
}

/// Who may use this deployment, and which key it uses on their behalf.
///
/// Off by default, and off is the trade every earlier build made: the client
/// sends a key of its own and this process forwards it. On is the other trade —
/// the key lives here and on no client, each caller is given a token of its own,
/// and a token can be limited or revoked without touching anyone else's. That is
/// what makes the deployment something to hand out rather than a copy of a key
/// handed out with it.
///
/// The two are exclusive rather than layered: a deployment that issues tokens
/// refuses a `user_…` key, because a key that still works is a key no revocation
/// reaches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccessConfig {
    /// Issue tokens instead of forwarding the client's key.
    pub enabled: bool,
    /// The file the upstream key is read from, when this deployment issues tokens.
    ///
    /// Either a JSON object with an `apiKey` field — what `cmdc login` writes — or
    /// a file whose whole content is the key. Read at startup and on change, never
    /// written to, so the key is not copied into this configuration, into a printed
    /// configuration, or into a log line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_file: Option<PathBuf>,
    /// Where issued tokens are recorded.
    ///
    /// Tokens arrive hashed, so this file is not a credential store: it can be
    /// read, backed up and diffed without handing anyone the ability to spend the
    /// account. `--token-new` writes it, the serving process reads it, and a
    /// change to it is picked up without a restart.
    pub tokens_file: PathBuf,
}

impl Default for AccessConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            key_file: None,
            tokens_file: PathBuf::from("var/tokens.json"),
        }
    }
}

/// Device-identity input.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FingerprintConfig {
    /// Rotates the derived identity in bulk. Empty means the built-in rule.
    pub salt: String,
}

/// Log rendering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Json,
    Text,
}

impl LogFormat {
    /// The name as it is configured and as it is written in a line.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            LogFormat::Json => "json",
            LogFormat::Text => "text",
        }
    }
}

/// How much of the log a deployment keeps.
///
/// Ordered from quietest to loudest, so that "does this message clear the
/// configured level" is a comparison rather than a set of cases.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
}

impl LogLevel {
    /// The name as it is configured and as it is written in a line.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
        }
    }
}

impl std::str::FromStr for LogLevel {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.trim().to_ascii_lowercase().as_str() {
            "error" => Ok(LogLevel::Error),
            "warn" => Ok(LogLevel::Warn),
            "info" => Ok(LogLevel::Info),
            "debug" => Ok(LogLevel::Debug),
            other => Err(format!(
                "unknown log level {other:?}; expected error, warn, info or debug"
            )),
        }
    }
}

/// Observability settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelemetryConfig {
    pub log_level: LogLevel,
    pub log_format: LogFormat,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            log_level: LogLevel::Info,
            log_format: LogFormat::Json,
        }
    }
}
