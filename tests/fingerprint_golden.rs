//! Byte-for-byte parity with the JavaScript implementation.
//!
//! The fixture is produced by running the original code, not by re-deriving the
//! algorithm. A failure here means accounts would silently change device.

use bifrost::fingerprint::{DeviceProfile, Fingerprint, generate_fingerprint};
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    fp_salt: String,
    device_profile: FixtureProfile,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct FixtureProfile {
    platform: String,
    arch: String,
    #[serde(rename = "osRelease")]
    os_release: String,
    #[serde(rename = "isContainer")]
    is_container: bool,
    #[serde(rename = "projectDir")]
    project_dir: String,
}

#[derive(Deserialize)]
struct Case {
    api_key: String,
    salt: String,
    expected: serde_json::Value,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("../fixtures/fingerprint_golden.json")).expect("fixture parses")
}

fn profile_of(fixture: &Fixture) -> DeviceProfile {
    DeviceProfile {
        platform: fixture.device_profile.platform.clone(),
        arch: fixture.device_profile.arch.clone(),
        os_release: fixture.device_profile.os_release.clone(),
        is_container: fixture.device_profile.is_container,
        project_dir: fixture.device_profile.project_dir.clone(),
    }
}

#[test]
fn every_golden_case_matches_byte_for_byte() {
    let fixture = fixture();
    let profile = profile_of(&fixture);
    assert!(!fixture.cases.is_empty(), "fixture must contain cases");

    for case in &fixture.cases {
        let derived = generate_fingerprint(&case.api_key, &case.salt, &profile);
        let actual = serde_json::to_value(&derived).expect("serialize");
        assert_eq!(
            actual, case.expected,
            "fingerprint diverged for api_key={:?} salt={:?}",
            case.api_key, case.salt
        );
    }
}

#[test]
fn the_hashing_salt_is_unchanged() {
    let fixture = fixture();
    assert_eq!(fixture.fp_salt, "command-code:device-fingerprint:v1");
}

#[test]
fn the_same_key_always_reports_the_same_device() {
    let profile = DeviceProfile::default();
    let first: Fingerprint = generate_fingerprint("user_stable", "", &profile);
    let second = generate_fingerprint("user_stable", "", &profile);
    assert_eq!(first, second);
}

#[test]
fn different_keys_report_different_devices() {
    let profile = DeviceProfile::default();
    let one = generate_fingerprint("user_one", "", &profile);
    let two = generate_fingerprint("user_two", "", &profile);
    assert_ne!(one.thumbmark, two.thumbmark);
}

#[test]
fn the_salt_rotates_the_identity() {
    let profile = DeviceProfile::default();
    let original = generate_fingerprint("user_stable", "", &profile);
    let rotated = generate_fingerprint("user_stable", "pepper-1", &profile);
    assert_ne!(original.thumbmark, rotated.thumbmark);
}

#[test]
fn hashes_are_lowercase_hex_and_signals_are_never_absent() {
    let profile = DeviceProfile::default();
    let derived = generate_fingerprint("user_abc123", "", &profile);

    assert_eq!(derived.thumbmark.len(), 64);
    assert!(
        derived
            .thumbmark
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
    assert!(derived.components.machine_id_hash.is_some());
    assert!(derived.components.os_user_hash.is_some());
    assert!(derived.components.hostname_hash.is_some());
    assert!(derived.components.git_email_hash.is_some());
    assert!(!derived.components.mac_hashes.is_empty());
}

#[test]
fn the_reported_adapter_count_is_within_the_candidate_range() {
    let profile = DeviceProfile::default();
    let derived = generate_fingerprint("user_abc123", "", &profile);
    let macs = &derived.components.mac_hashes;

    assert!(
        (2..=5).contains(&macs.len()),
        "expected 2..=5 adapters, got {}",
        macs.len()
    );
    assert!(
        macs.iter()
            .all(|mac| mac.len() == 64 && mac.chars().all(|c| c.is_ascii_hexdigit()))
    );
}

#[test]
fn the_profile_is_reported_verbatim() {
    let fixture = fixture();
    let profile = profile_of(&fixture);
    let derived = generate_fingerprint("user_abc123", "", &profile);
    assert_eq!(derived.components.platform, profile.platform);
    assert_eq!(derived.components.arch, profile.arch);
    assert_eq!(derived.components.os_release, profile.os_release);
    assert_eq!(derived.components.is_container, profile.is_container);
    assert_eq!(derived.components.runtime, "cli");
    assert_eq!(derived.components.collector_version, 1);
}
