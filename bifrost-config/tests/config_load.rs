//! Configuration parsing, validation and layering.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use bifrost_config::{
    AuditConfig, CONFIG_VERSION, Config, ConfigError, FingerprintConfig, LogFormat, LogLevel, MechanismsConfig,
    REDACTED, apply_env, load_from,
};

fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn with_env(mut config: Config, pairs: &[(&str, &str)]) -> Config {
    let map = env(pairs);
    apply_env(&mut config, |key| map.get(key).cloned()).expect("env applies");
    config
}

#[test]
fn defaults_match_the_documented_values() {
    let config = Config::default();
    assert_eq!(config.version, CONFIG_VERSION);
    assert_eq!(config.port, 3050);
    assert_eq!(config.host, "0.0.0.0");
    assert_eq!(config.api_base, "https://api.commandcode.ai");
    assert_eq!(config.device.project_dir, None);
    assert_eq!(config.wire.adapter, "cc/1.53.1");
    assert!(config.wire.drift_watch);
    assert_eq!(config.wire.drift_registry, "https://registry.npmjs.org");
    assert!(!config.wire.zdr);
    assert_eq!(config.limits.max_body_mb, 100);
    assert_eq!(config.limits.stream_idle_ms, 30_000);
    assert_eq!(config.limits.nonstream_idle_ms, 90_000);
    assert_eq!(config.limits.max_inflight, 0);
    assert_eq!(config.limits.announce_ms, 10_000);
    assert!(config.models.provider);
    assert_eq!(config.models.refresh_ms, 300_000);
    assert_eq!(config.models.timeout_ms, 10_000);
    assert_eq!(config.telemetry.log_level, LogLevel::Info);
    assert_eq!(config.telemetry.log_format, LogFormat::Json);
    config.validate().expect("defaults are valid");
}

#[test]
fn a_partial_file_inherits_every_other_default() {
    let config = Config::from_toml_str("port = 4000\n[limits]\nmax_body_mb = 8\n").expect("parses");
    assert_eq!(config.port, 4000);
    assert_eq!(config.limits.max_body_mb, 8);
    assert_eq!(config.limits.stream_idle_ms, 30_000);
    assert_eq!(config.wire.adapter, "cc/1.53.1");
}

#[test]
fn an_unknown_top_level_key_is_rejected() {
    let error = Config::from_toml_str("prot = 4000\n").expect_err("typo must fail");
    assert!(matches!(error, ConfigError::Parse { .. }), "got {error:?}");
}

#[test]
fn an_unknown_nested_key_is_rejected() {
    let error = Config::from_toml_str("[mechanisms]\nfingerprintt = true\n").expect_err("typo must fail");
    assert!(matches!(error, ConfigError::Parse { .. }), "got {error:?}");
}

#[test]
fn a_future_schema_version_is_rejected() {
    let error = Config::from_toml_str("version = 2\n").expect_err("unknown version must fail");
    match error {
        ConfigError::Invalid(message) => assert!(message.contains("version must be 1"), "{message}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn a_malformed_adapter_is_rejected() {
    for adapter in ["cc", "cc/1.53", "cc/1.53.1.2", "/1.53.1", "cc/x.y.z", ""] {
        let text = format!("[wire]\nadapter = {adapter:?}\n");
        assert!(
            Config::from_toml_str(&text).is_err(),
            "adapter {adapter:?} should be rejected"
        );
    }
}

#[test]
fn a_well_formed_adapter_parses_into_name_and_version() {
    let config = Config::from_toml_str("[wire]\nadapter = \"cc/1.53.1\"\n").expect("parses");
    let adapter = config.wire.parse_adapter().expect("parses");
    assert_eq!(adapter.name, "cc");
    assert_eq!(adapter.version, "1.53.1");
}

/// The level is a closed vocabulary rather than a string nothing reads: a
/// deployment that misspells it is told at startup, not never.
/// The working directory is the one part of the device profile an operator
/// chooses, and the profile is what everything else about the machine comes from.
#[test]
fn the_device_working_directory_keeps_its_old_variable_name() {
    let config = with_env(Config::default(), &[("CC_DEVICE_PROJECT_DIR", "D:\\code\\thing")]);
    assert_eq!(config.device.project_dir.as_deref(), Some("D:\\code\\thing"));

    let config = Config::from_toml_str("[device]\nproject_dir = \"/srv/app\"\n").expect("parses");
    assert_eq!(config.device.project_dir.as_deref(), Some("/srv/app"));
}

#[test]
fn an_empty_device_working_directory_is_rejected() {
    assert!(Config::from_toml_str("[device]\nproject_dir = \"  \"\n").is_err());
    let mut config = Config::default();
    apply_env(&mut config, |key| (key == "CC_DEVICE_PROJECT_DIR").then(String::new)).expect("env applies");
    let error = config
        .validate()
        .expect_err("an empty working directory must fail rather than be replaced");
    assert!(matches!(error, ConfigError::Invalid(_)), "got {error:?}");
}

/// Retention is configured in the units an operator keeps their evidence in, and a
/// deployment that asked for none asks for none: both spellings of "keep it" are
/// the same absence, so nothing is pruned by a default.
#[test]
fn archive_retention_is_configured_in_days_and_megabytes() {
    let config = Config::from_toml_str("[audit]\nretain_days = 30\nmax_total_mb = 4096\n").expect("parses");
    assert_eq!(
        config.audit.retain_age(),
        Some(std::time::Duration::from_secs(30 * 24 * 60 * 60))
    );
    assert_eq!(config.audit.max_total_bytes(), Some(4096 * 1024 * 1024));
    assert!(config.audit.prunes());

    let config = AuditConfig::default();
    assert_eq!(config.retain_age(), None);
    assert_eq!(config.max_total_bytes(), None);
    assert!(!config.prunes(), "the default keeps everything and prunes nothing");
}

/// A retention that deletes is the place a mistyped digit is worst, so the bounds
/// are checked before a pass rather than by one.
#[test]
fn an_implausible_retention_is_rejected() {
    let error = Config::from_toml_str("[audit]\nretain_days = 3651\n").expect_err("too long");
    assert!(error.to_string().contains("retain_days"), "{error}");

    let error = Config::from_toml_str("[audit]\nmax_total_mb = 1048577\n").expect_err("too large");
    assert!(error.to_string().contains("max_total_mb"), "{error}");

    // The bounds themselves are accepted, so the check is a bound and not a ban.
    Config::from_toml_str("[audit]\nretain_days = 3650\nmax_total_mb = 1048576\n").expect("the bounds pass");
}

#[test]
fn a_log_level_is_one_of_four_words() {
    for (text, expected) in [
        ("error", LogLevel::Error),
        ("warn", LogLevel::Warn),
        ("info", LogLevel::Info),
        ("debug", LogLevel::Debug),
    ] {
        let config = Config::from_toml_str(&format!("[telemetry]\nlog_level = \"{text}\"\n")).expect("parses");
        assert_eq!(config.telemetry.log_level, expected);
    }
    assert!(Config::from_toml_str("[telemetry]\nlog_level = \"loud\"\n").is_err());
    assert!(Config::from_toml_str("[telemetry]\nlog_level = \"\"\n").is_err());
}

#[test]
fn an_unparsable_log_level_in_the_environment_is_rejected() {
    let error = apply_env(&mut Config::default(), |key| {
        (key == "BIFROST_LOG_LEVEL").then(|| "verbose".to_owned())
    })
    .expect_err("word salad must fail");
    match error {
        ConfigError::Invalid(message) => assert!(message.contains("verbose"), "{message}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
    let config = with_env(Config::default(), &[("BIFROST_LOG_LEVEL", "debug")]);
    assert_eq!(config.telemetry.log_level, LogLevel::Debug);
}

#[test]
fn a_registry_that_is_not_a_url_is_rejected() {
    assert!(Config::from_toml_str("[wire]\ndrift_registry = \"registry.npmjs.org\"\n").is_err());
    assert!(Config::from_toml_str("[wire]\ndrift_registry = \"\"\n").is_err());
    assert!(Config::from_toml_str("[wire]\ndrift_registry = \"https://mirror.test\"\n").is_ok());
}

/// The knob that let a deployment report a version it did not implement is gone
/// rather than left unread: it described a thing this proxy never does.
#[test]
fn the_version_is_not_a_switch() {
    let error = Config::from_toml_str("[wire]\npin_version = false\n").expect_err("the key is gone");
    assert!(matches!(error, ConfigError::Parse { .. }), "got {error:?}");
}

#[test]
fn port_zero_is_rejected() {
    assert!(Config::from_toml_str("port = 0\n").is_err());
}

#[test]
fn api_base_must_carry_a_scheme() {
    assert!(Config::from_toml_str("api_base = \"api.commandcode.ai\"\n").is_err());
    assert!(Config::from_toml_str("api_base = \"http://localhost:9000\"\n").is_ok());
}

#[test]
fn zero_timeouts_are_rejected() {
    assert!(Config::from_toml_str("[limits]\nstream_idle_ms = 0\n").is_err());
    assert!(Config::from_toml_str("[limits]\nnonstream_idle_ms = 0\n").is_err());
    assert!(Config::from_toml_str("[limits]\nmax_body_mb = 0\n").is_err());
    assert!(Config::from_toml_str("[limits]\nannounce_ms = 0\n").is_err());
    assert!(Config::from_toml_str("[models]\ntimeout_ms = 0\n").is_err());
    // Not a timeout: `0` here means every request refetches, which is a policy
    // rather than a mistake.
    Config::from_toml_str("[models]\nrefresh_ms = 0\n").expect("a zero refresh window is allowed");
}

#[test]
fn environment_overrides_win_over_the_file() {
    let config = Config::from_toml_str("port = 4000\napi_base = \"https://example.test\"\n").expect("parses");
    let config = with_env(
        config,
        &[
            ("PORT", "5000"),
            ("HOST", "127.0.0.1"),
            ("CC_API_BASE", "https://other.test"),
        ],
    );
    assert_eq!(config.port, 5000);
    assert_eq!(config.host, "127.0.0.1");
    assert_eq!(config.api_base, "https://other.test");
}

#[test]
fn zdr_accepts_the_spellings_the_proxy_used() {
    for value in ["1", "true", "TRUE", "yes"] {
        let config = with_env(Config::default(), &[("CMD_ZDR", value)]);
        assert!(config.wire.zdr, "CMD_ZDR={value} should enable zdr");
    }
    for value in ["0", "false", "", "no"] {
        let config = with_env(Config::default(), &[("CMD_ZDR", value)]);
        assert!(!config.wire.zdr, "CMD_ZDR={value} should not enable zdr");
    }
}

#[test]
fn the_provider_catalogue_switch_keeps_its_old_variable_name() {
    let off = with_env(Config::default(), &[("CC_USE_PROVIDER_MODELS", "false")]);
    assert!(!off.models.provider);
    // Anything else leaves it on, as the original reads it: an unreadable value
    // must not silently disable a mechanism.
    for value in ["true", "1", "yes", "nonsense"] {
        let config = with_env(Config::default(), &[("CC_USE_PROVIDER_MODELS", value)]);
        assert!(config.models.provider, "CC_USE_PROVIDER_MODELS={value}");
    }
}

#[test]
fn the_fingerprint_salt_keeps_its_old_variable_name() {
    let config = with_env(Config::default(), &[("CC_FINGERPRINT_SALT", "pepper-1")]);
    assert_eq!(config.fingerprint.salt, "pepper-1");
}

#[test]
fn an_unparsable_port_is_rejected_instead_of_ignored() {
    let error = apply_env(&mut Config::default(), |key| {
        (key == "PORT").then(|| "not-a-number".to_owned())
    })
    .expect_err("bad port must fail");
    match error {
        ConfigError::Invalid(message) => assert!(message.contains("PORT"), "{message}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn blank_environment_values_leave_defaults_alone() {
    let config = with_env(Config::default(), &[("HOST", "   "), ("CC_API_BASE", "")]);
    assert_eq!(config.host, "0.0.0.0");
    assert_eq!(config.api_base, "https://api.commandcode.ai");
}

#[test]
fn the_checked_in_example_config_is_valid() {
    let text = include_str!("../../bifrost.example.toml");
    let config = Config::from_toml_str(text).expect("bifrost.example.toml must stay valid");
    assert_eq!(config.limits.max_body_mb, 100);
    assert!(config.mechanisms.fingerprint);
    assert!(!config.mechanisms.evidence_archive);
    assert!(config.models.provider);
    assert!(config.wire.drift_watch);
    assert_eq!(config.wire.drift_registry, "https://registry.npmjs.org");
    assert_eq!(config.telemetry.log_level, LogLevel::Info);
    assert_eq!(config.device.project_dir, None);
}

#[test]
fn limits_expose_bytes_and_per_request_timeouts() {
    let config = Config::default();
    assert_eq!(config.limits.max_body_bytes(), 100 * 1024 * 1024);
    assert_eq!(config.limits.idle_for(true), config.limits.stream_idle());
    assert_eq!(config.limits.idle_for(false), config.limits.nonstream_idle());
}

/// A file of its own per call, so tests running at once cannot see each other's.
fn config_file(text: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let name = format!(
        "bifrost-config-{}-{}.toml",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, text).expect("the file is written");
    path
}

/// A file named by the caller is the file that is read, which is what makes
/// `--check --config candidate.toml` a question about a configuration that is not
/// installed yet.
#[test]
fn a_named_file_is_the_one_that_is_read() {
    let file = config_file("port = 4055\n");
    assert_eq!(load_from(Some(&file)).expect("the named file is read").port, 4055);
}

/// The printed configuration is the resolved one, and it is a configuration: what
/// is on screen can be read back rather than merely read.
#[test]
fn a_printed_configuration_parses_back_into_the_same_thing() {
    let config = Config {
        port: 4055,
        mechanisms: MechanismsConfig {
            evidence_archive: true,
            ..MechanismsConfig::default()
        },
        audit: AuditConfig {
            journal_dir: PathBuf::from("/var/lib/bifrost/journal"),
            ..AuditConfig::default()
        },
        ..Config::default()
    };
    let printed = config.to_toml_redacted().expect("renders");
    assert_eq!(Config::from_toml_str(&printed).expect("parses back"), config);
}

/// TOML has no null, and a printed file that could not be read back would not be a
/// configuration. An unset working directory is absent instead.
#[test]
fn an_unset_working_directory_is_absent_rather_than_null() {
    let printed = Config::default().to_toml_redacted().expect("renders");
    assert!(!printed.contains("project_dir"), "{printed}");
}

/// The salt rotates a deployment's whole device identity. It goes out redacted for
/// the same reason the journal keeps a key's fingerprint rather than the key.
#[test]
fn every_secret_in_a_printed_configuration_is_replaced() {
    let config = Config {
        fingerprint: FingerprintConfig {
            salt: "rotates-every-identity".to_owned(),
        },
        ..Config::default()
    };
    let printed = config.to_toml_redacted().expect("renders");
    assert!(!printed.contains("rotates-every-identity"), "{printed}");
    assert!(printed.contains(&format!("salt = \"{REDACTED}\"")), "{printed}");
}
