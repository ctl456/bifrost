//! The HTTP edge.
//!
//! Routing, admission control, the three public endpoints and the streaming path
//! between them and the upstream.

#![forbid(unsafe_code)]

pub mod auth;
pub mod cli;
pub mod drift;
pub mod error;
pub mod evidence;
pub mod lifecycle;
pub mod log;
pub mod models;
pub mod retention;
pub mod routes;
pub mod state;
pub mod stream;
pub mod upstream;

pub use routes::app;
pub use state::Edge;
