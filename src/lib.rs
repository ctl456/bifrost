//! Bifrost.
//!
//! One binary, one library, one job: accept the protocol a client speaks, speak
//! Command Code's own dialect to the upstream, and translate between them.
//!
//! The module split is the shape of that job rather than a set of packages.
//! [`core`] holds the request and response types every protocol is translated
//! into, so no adapter knows another adapter's schema; [`protocol`] turns those
//! into the three public surfaces; [`wire`] is the upstream side of the same
//! conversation, and [`fingerprint`] is the device identity it carries. Nothing
//! below [`edge`] knows about HTTP: the gateway, its configuration and its
//! logging live there, and [`config`] is what the operator writes.

#![forbid(unsafe_code)]

pub mod config;
pub mod core;
pub mod edge;
pub mod fingerprint;
pub mod protocol;
pub mod wire;
