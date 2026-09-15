//! Wire-level value construction.

use bifrost_wire::{Entropy, SequenceEntropy, SystemEntropy, Traceparent, is_uuid, slugify_path, today_utc, uuid_v4};

#[test]
fn paths_slugify_the_way_the_client_does() {
    assert_eq!(
        slugify_path("C:\\Users\\dev\\projects\\app"),
        "c-users-dev-projects-app"
    );
    assert_eq!(slugify_path("/home/dev/app"), "home-dev-app");
    assert_eq!(slugify_path("My Project!!"), "my-project");
    assert_eq!(slugify_path("--leading-and-trailing--"), "leading-and-trailing");
    assert_eq!(slugify_path("a///b"), "a-b");
    assert_eq!(slugify_path(""), "root");
    assert_eq!(slugify_path("!!!"), "root");
    assert_eq!(slugify_path("项目"), "root");
    assert_eq!(slugify_path("MixedCASE123"), "mixedcase123");
}

#[test]
fn uuids_are_version_four_and_canonically_shaped() {
    let uuid = uuid_v4(&SequenceEntropy::new(0));
    assert_eq!(uuid.len(), 36);
    assert!(is_uuid(&uuid), "{uuid} should parse as a UUID");

    let version = uuid.chars().nth(14).expect("version nibble");
    assert_eq!(version, '4', "version nibble must be 4");
    let variant = uuid.chars().nth(19).expect("variant nibble");
    assert!(matches!(variant, '8' | '9' | 'a' | 'b'), "variant nibble was {variant}");
}

#[test]
fn uuid_detection_rejects_anything_else() {
    assert!(is_uuid("11111111-2222-4333-8444-555555555555"));
    assert!(!is_uuid(""));
    assert!(!is_uuid("not-a-uuid"));
    assert!(!is_uuid("11111111-2222-4333-8444-55555555555"));
    assert!(!is_uuid("11111111-2222-4333-8444-55555555555z"));
    assert!(!is_uuid("11111111222243338444555555555555"));
    assert!(!is_uuid("11111111-2222-4333-8444-555555555555-extra"));
}

#[test]
fn a_traceparent_is_shaped_like_a_w3c_header() {
    let traceparent = Traceparent::generate(&SequenceEntropy::new(1));
    let rendered = traceparent.to_string();
    let parts: Vec<&str> = rendered.split('-').collect();

    assert_eq!(parts.len(), 4);
    assert_eq!(parts[0], "00", "version");
    assert_eq!(parts[1].len(), 32, "trace id");
    assert_eq!(parts[2].len(), 16, "parent id");
    assert_eq!(parts[3], "01", "sampled flag");
    assert!(rendered.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
}

#[test]
fn generated_values_do_not_repeat() {
    let entropy = SystemEntropy::new();
    let first = uuid_v4(&entropy);
    let second = uuid_v4(&entropy);
    assert_ne!(first, second, "the counter must make consecutive calls distinct");

    let traceparent = Traceparent::generate(&entropy);
    assert_ne!(
        traceparent.trace_id(),
        traceparent.parent_id(),
        "ids come from separate draws"
    );
}

#[test]
fn entropy_fills_the_whole_buffer() {
    let entropy = SequenceEntropy::new(3);
    let mut small = [0u8; 8];
    let mut large = [0u8; 80];
    entropy.fill(&mut small);
    entropy.fill(&mut large);

    assert!(small.iter().any(|byte| *byte != 0));
    assert!(large.iter().any(|byte| *byte != 0));
    assert_ne!(&large[0..8], &small[..], "consecutive draws must differ");
}

#[test]
fn today_is_an_iso_date() {
    let today = today_utc();
    assert_eq!(today.len(), 10);
    let mut parts = today.split('-');
    assert_eq!(parts.next().map(str::len), Some(4));
    assert_eq!(parts.next().map(str::len), Some(2));
    assert_eq!(parts.next().map(str::len), Some(2));
    assert!(today.chars().all(|c| c.is_ascii_digit() || c == '-'));
}
