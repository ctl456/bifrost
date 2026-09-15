//! Candidate pools the fingerprint draws from.
//!
//! These mirror the original implementation exactly. Changing any entry (or its
//! order) changes which device a given key reports, so additions are only safe
//! when they win the deterministic pick for keys that had no better candidate.

/// Root salt the upstream hashes with.
pub const FP_SALT: &str = "command-code:device-fingerprint:v1";

/// CPU model and core count, as reported on Windows x64.
pub const CPUS: &[(&str, u16)] = &[
    ("12th Gen Intel(R) Core(TM) i7-12650H", 10),
    ("12th Gen Intel(R) Core(TM) i5-12400F", 6),
    ("12th Gen Intel(R) Core(TM) i9-12900K", 16),
    ("13th Gen Intel(R) Core(TM) i7-13700K", 16),
    ("13th Gen Intel(R) Core(TM) i5-13600K", 14),
    ("13th Gen Intel(R) Core(TM) i9-13900K", 24),
    ("Intel(R) Core(TM) Ultra 7 155H", 16),
    ("Intel(R) Core(TM) Ultra 9 285H", 16),
    ("Intel(R) Core(TM) i9-14900K", 24),
    ("Intel(R) Core(TM) i7-14700K", 20),
    ("AMD Ryzen 7 7800X3D", 8),
    ("AMD Ryzen 9 7950X", 16),
    ("AMD Ryzen 5 7600", 6),
    ("AMD Ryzen 9 7900X", 12),
    ("AMD Ryzen 7 5800X3D", 8),
];

/// Installed memory, in GiB.
pub const MEMS_GIB: &[u16] = &[8, 16, 24, 32, 48, 64];

/// IANA time zones.
pub const TIMEZONES: &[&str] = &[
    "America/New_York",
    "America/Chicago",
    "America/Los_Angeles",
    "America/Toronto",
    "Europe/London",
    "Europe/Berlin",
    "Europe/Paris",
    "Europe/Moscow",
    "Asia/Shanghai",
    "Asia/Tokyo",
    "Asia/Singapore",
    "Asia/Seoul",
    "Asia/Hong_Kong",
    "Australia/Sydney",
    "Pacific/Auckland",
];

/// How many network adapters the reported machine has.
pub const MAC_COUNTS: &[u8] = &[2, 3, 4, 5];

/// Local account names.
pub const OS_USERS: &[&str] = &["dev", "user", "admin", "coder", "engineer", "work"];

/// Mail domains for the git identity.
pub const MAIL_DOMAINS: &[&str] = &["gmail.com", "outlook.com", "qq.com", "163.com"];
