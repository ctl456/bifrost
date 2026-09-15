//! Environment-variable overrides.
//!
//! Variable names keep the spelling the original proxy used, so existing
//! deployments do not need a new set of secrets or a rewritten unit file.

use crate::ConfigError;
use crate::model::Config;

/// Apply environment overrides in place.
///
/// `get` is injected rather than read from the process so tests stay hermetic.
pub fn apply_env(config: &mut Config, get: impl Fn(&str) -> Option<String>) -> Result<(), ConfigError> {
    if let Some(value) = trimmed(&get, "PORT") {
        config.port = value
            .parse()
            .map_err(|_| ConfigError::Invalid(format!("PORT must be a number, got {value:?}")))?;
    }
    if let Some(value) = trimmed(&get, "HOST") {
        config.host = value;
    }
    if let Some(value) = trimmed(&get, "CC_API_BASE") {
        config.api_base = value;
    }
    if let Some(value) = get("CC_DEVICE_PROJECT_DIR").map(|value| value.trim().to_owned()) {
        // Not `trimmed`: an empty value is a deliberate `Some("")` so that the
        // validation rejects it, rather than falling back to the built-in profile
        // the operator was trying to replace.
        config.device.project_dir = Some(value);
    }
    if let Some(value) = trimmed(&get, "BIFROST_LOG_LEVEL") {
        config.telemetry.log_level = value
            .parse()
            .map_err(|error: String| ConfigError::Invalid(format!("BIFROST_LOG_LEVEL {error}, got {value:?}")))?;
    }
    if let Some(value) = trimmed(&get, "CC_FINGERPRINT_SALT") {
        config.fingerprint.salt = value;
    }
    if let Some(value) = trimmed(&get, "CC_USE_PROVIDER_MODELS") {
        // Spelled as the original spells it: anything but `false` leaves the
        // catalogue on, so an unreadable value does not silently disable it.
        config.models.provider = value != "false";
    }
    if let Some(value) = get("CMD_ZDR") {
        config.wire.zdr = matches!(value.trim(), "1" | "true" | "TRUE" | "yes");
    }
    Ok(())
}

fn trimmed(get: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    let value = get(key)?;
    let value = value.trim().to_owned();
    (!value.is_empty()).then_some(value)
}
