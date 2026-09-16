//! Configuration parsing, validation and layering.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use bifrost::config::{
    CONFIG_VERSION, Config, ConfigError, FingerprintConfig, LogFormat, LogLevel, MechanismsConfig, REDACTED, apply_env,
    load_from,
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
    assert!(config.models.aliases.is_empty(), "no rules means no rewriting");
    assert!(!config.access.enabled, "the key is forwarded unless asked otherwise");
    assert_eq!(config.access.key_file, None);
    assert_eq!(config.access.tokens_file, PathBuf::from("var/tokens.json"));
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

/// A rule points a name a client asks for at a model the account is served.
///
/// The longest matching pattern wins, which is the whole of the semantics: a
/// family rule must not swallow the member of that family which has a rule of its
/// own, and an exact pattern is the longest pattern that can match its own name.
#[test]
fn a_model_rule_rewrites_the_names_it_matches() {
    let config = Config::from_toml_str(
        r#"
        [models.aliases]
        "claude-" = "deepseek/deepseek-v4-flash"
        "claude-haiku-" = "deepseek/deepseek-v4-pro"
        "gpt-5.4" = "deepseek/deepseek-v4-pro"
        "#,
    )
    .expect("parses");

    assert_eq!(
        config.models.resolve("claude-opus-4-6"),
        Some("deepseek/deepseek-v4-flash")
    );
    assert_eq!(
        config.models.resolve("claude-haiku-4-5-20251001"),
        Some("deepseek/deepseek-v4-pro"),
        "the rule that named this model is the one that answers for it"
    );
    assert_eq!(config.models.resolve("gpt-5.4"), Some("deepseek/deepseek-v4-pro"));
    assert_eq!(config.models.resolve("gpt-5.4-mini"), Some("deepseek/deepseek-v4-pro"));
    assert_eq!(
        config.models.resolve("Claude-Haiku-4-5"),
        Some("deepseek/deepseek-v4-pro"),
        "a client's capitalisation is not a different model"
    );
    assert_eq!(
        config.models.resolve("claude"),
        None,
        "a prefix is a prefix: it does not match a name that stops short of it"
    );
    assert_eq!(
        config.models.resolve("gpt-5.4x"),
        Some("deepseek/deepseek-v4-pro"),
        "and a name that runs past it is still covered"
    );
    assert_eq!(
        config.models.resolve("deepseek/deepseek-v4-flash"),
        None,
        "an unmapped name is left for the upstream to answer for"
    );

    assert_eq!(Config::default().models.resolve("claude-sonnet-5"), None);
}

/// A rule that matches everything, or sends nothing, is a typo that would only be
/// visible as the wrong model answering, so it never loads.
#[test]
fn a_model_rule_that_could_not_name_a_model_is_rejected() {
    assert!(Config::from_toml_str("[models.aliases]\n\"\" = \"deepseek/deepseek-v4-flash\"\n").is_err());
    assert!(Config::from_toml_str("[models.aliases]\n\"claude-\" = \"\"\n").is_err());
    assert!(Config::from_toml_str("[models.aliases]\n\"claude-\" = \"   \"\n").is_err());
    Config::from_toml_str("[models.aliases]\n\"claude-\" = \"deepseek/deepseek-v4-flash\"\n")
        .expect("a rule that names a model loads");
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
    let text = include_str!("../bifrost.example.toml");
    let config = Config::from_toml_str(text).expect("bifrost.example.toml must stay valid");
    assert_eq!(config.limits.max_body_mb, 100);
    assert!(config.mechanisms.fingerprint);
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
        "bifrost-{}-{}.toml",
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
            prompt_cache: true,
            ..MechanismsConfig::default()
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
/// the same reason the access line keeps a key's fingerprint rather than the key.
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
/// A deployment that issues tokens has to say where the key comes from: with the
/// switch on and no file, the process would start and refuse every request, which
/// is a mistake worth failing on rather than serving through.
#[test]
fn issuing_tokens_without_a_key_file_is_rejected() {
    let error = Config::from_toml_str("[access]\nenabled = true\n").expect_err("no key file");
    match error {
        ConfigError::Invalid(message) => assert!(message.contains("access.key_file is required"), "{message}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn an_access_section_round_trips_through_a_printed_configuration() {
    let text =
        "[access]\nenabled = true\nkey_file = \"/run/secrets/commandcode.json\"\ntokens_file = \"var/tokens.json\"\n";
    let config = Config::from_toml_str(text).expect("parses");
    assert!(config.access.enabled);
    assert_eq!(
        config.access.key_file,
        Some(PathBuf::from("/run/secrets/commandcode.json"))
    );

    let printed = config.to_toml_redacted().expect("prints");
    let back = Config::from_toml_str(&printed).expect("what is printed is a configuration");
    assert_eq!(back.access, config.access);
    // The key is named by path rather than held, so there is no secret in this
    // section for the redaction to reach — and the path has to survive, because it
    // is what the deployment opens.
    assert!(printed.contains("/run/secrets/commandcode.json"));
}

#[test]
fn an_access_section_with_an_empty_path_is_rejected() {
    for text in [
        "[access]\nenabled = true\nkey_file = \"\"\n",
        "[access]\ntokens_file = \"\"\n",
    ] {
        assert!(Config::from_toml_str(text).is_err(), "{text:?} names no file");
    }
}

#[test]
fn an_unknown_key_in_the_access_section_is_rejected() {
    let error = Config::from_toml_str("[access]\nenabled = true\nkeyfile = \"x\"\n").expect_err("typo");
    assert!(matches!(error, ConfigError::Parse { .. }), "got {error:?}");
}
