//! Layered, strictly validated configuration.
//!
//! Precedence is defaults, then the TOML file, then environment variables.
//! Unknown keys are rejected rather than ignored: a typo in a feature flag must
//! fail loudly instead of silently leaving a mechanism disabled.

#![forbid(unsafe_code)]

mod env;
mod model;

pub use env::apply_env;
pub use model::{
    AdapterId, AuditConfig, CONFIG_VERSION, Config, DeviceConfig, FingerprintConfig, LimitsConfig, LogFormat, LogLevel,
    MechanismsConfig, ModelsConfig, TelemetryConfig, WireConfig,
};

use std::path::{Path, PathBuf};

/// The file Bifrost looks for when `BIFROST_CONFIG` is unset.
pub const CONFIG_FILE_NAME: &str = "bifrost.toml";

/// What is printed where a secret would be.
pub const REDACTED: &str = "<redacted>";

/// The longest archive retention that is taken seriously, in days.
pub const MAX_AUDIT_RETAIN_DAYS: u32 = 3650;

/// The largest archive ceiling that is taken seriously, in megabytes.
pub const MAX_AUDIT_TOTAL_MB: u64 = 1_048_576;

/// Everything that can go wrong while producing a validated configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid config: {0}")]
    Invalid(String),
    #[error("failed to render config: {0}")]
    Render(#[from] toml::ser::Error),
}

impl Config {
    /// Parse and validate a configuration from TOML text.
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: PathBuf::from("<inline>"),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Read, parse and validate a configuration file.
    pub fn from_path(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let config: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Reject configurations that would fail at runtime.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version != CONFIG_VERSION {
            return Err(ConfigError::Invalid(format!(
                "version must be {CONFIG_VERSION}, got {}",
                self.version
            )));
        }
        if self.port == 0 {
            return Err(ConfigError::Invalid("port must not be 0".to_owned()));
        }
        if self.host.trim().is_empty() {
            return Err(ConfigError::Invalid("host must not be empty".to_owned()));
        }
        if !self.api_base.starts_with("http://") && !self.api_base.starts_with("https://") {
            return Err(ConfigError::Invalid(format!(
                "api_base must be an http(s) URL, got {:?}",
                self.api_base
            )));
        }
        if self
            .device
            .project_dir
            .as_ref()
            .is_some_and(|dir| dir.trim().is_empty())
        {
            return Err(ConfigError::Invalid(
                "device.project_dir must not be empty when it is set".to_owned(),
            ));
        }
        if self.wire.parse_adapter().is_none() {
            return Err(ConfigError::Invalid(format!(
                "wire.adapter must look like \"<name>/<major>.<minor>.<patch>\", got {:?}",
                self.wire.adapter
            )));
        }
        if !self.wire.drift_registry.starts_with("http://") && !self.wire.drift_registry.starts_with("https://") {
            return Err(ConfigError::Invalid(format!(
                "wire.drift_registry must be an http(s) URL, got {:?}",
                self.wire.drift_registry
            )));
        }
        if self.limits.max_body_mb == 0 {
            return Err(ConfigError::Invalid("limits.max_body_mb must be at least 1".to_owned()));
        }
        if self.limits.stream_idle_ms == 0 {
            return Err(ConfigError::Invalid(
                "limits.stream_idle_ms must be greater than 0".to_owned(),
            ));
        }
        if self.limits.nonstream_idle_ms == 0 {
            return Err(ConfigError::Invalid(
                "limits.nonstream_idle_ms must be greater than 0".to_owned(),
            ));
        }
        if self.limits.announce_ms == 0 {
            return Err(ConfigError::Invalid(
                "limits.announce_ms must be greater than 0; turn mechanisms.lifecycle off to stop announcing"
                    .to_owned(),
            ));
        }
        if self.models.timeout_ms == 0 {
            return Err(ConfigError::Invalid(
                "models.timeout_ms must be greater than 0".to_owned(),
            ));
        }
        // The upper bounds are typos caught early rather than policy: a retention
        // longer than ten years is a mistyped digit, and so is a ceiling of a
        // petabyte — and both are only discovered by the pass that has already
        // deleted something.
        if self.audit.retain_days > MAX_AUDIT_RETAIN_DAYS {
            return Err(ConfigError::Invalid(format!(
                "audit.retain_days must be at most {MAX_AUDIT_RETAIN_DAYS}, got {}",
                self.audit.retain_days
            )));
        }
        if self.audit.max_total_mb > MAX_AUDIT_TOTAL_MB {
            return Err(ConfigError::Invalid(format!(
                "audit.max_total_mb must be at most {MAX_AUDIT_TOTAL_MB}, got {}",
                self.audit.max_total_mb
            )));
        }
        Ok(())
    }

    /// This configuration as TOML, with every secret replaced.
    ///
    /// The file an operator edited is one of three layers, so printing the file
    /// back answers what they already know; printing the resolved configuration is
    /// the question `--print-config` exists for. Secrets are replaced rather than
    /// printed because the output is meant to be pasteable into a report: the salt
    /// rotates a deployment's whole device identity, so it goes out as
    /// [`REDACTED`] the same way the journal keeps a key's fingerprint instead of
    /// the key.
    pub fn to_toml_redacted(&self) -> Result<String, ConfigError> {
        let mut value = toml::Value::try_from(self)?;
        if !self.fingerprint.salt.is_empty()
            && let Some(salt) = value
                .get_mut("fingerprint")
                .and_then(|fingerprint| fingerprint.get_mut("salt"))
        {
            *salt = toml::Value::String(REDACTED.to_owned());
        }
        Ok(toml::to_string_pretty(&value)?)
    }
}

/// Load the effective configuration from the environment.
///
/// Reads `BIFROST_CONFIG` (or `./bifrost.toml` when present), then applies
/// environment overrides, then validates.
pub fn load() -> Result<Config, ConfigError> {
    load_from(None)
}

/// [`load`], with the file named by the caller rather than chosen by the
/// environment.
///
/// A path named out loud wins over `BIFROST_CONFIG` and over `./bifrost.toml`:
/// whoever names a file is asking about that file, which is what makes
/// `--check --config candidate.toml` a question about the configuration that is
/// about to be installed instead of the one already there.
pub fn load_from(explicit: Option<&Path>) -> Result<Config, ConfigError> {
    load_with_path(explicit, |key| std::env::var(key).ok())
}

/// [`load`] with an injectable environment, so tests never mutate the process.
pub fn load_with(get: impl Fn(&str) -> Option<String>) -> Result<Config, ConfigError> {
    load_with_path(None, get)
}

fn load_with_path(explicit: Option<&Path>, get: impl Fn(&str) -> Option<String>) -> Result<Config, ConfigError> {
    let path = explicit
        .map(Path::to_path_buf)
        .or_else(|| get("BIFROST_CONFIG").map(PathBuf::from))
        .or_else(|| {
            let candidate = PathBuf::from(CONFIG_FILE_NAME);
            candidate.exists().then_some(candidate)
        });

    let mut config = match path {
        Some(path) => Config::from_path(&path)?,
        None => Config::default(),
    };
    apply_env(&mut config, &get)?;
    config.validate()?;
    Ok(config)
}
