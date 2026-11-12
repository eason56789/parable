//! Parable harness core — agent loop, providers, tools, permissions, sessions, local server.

pub mod agent;
pub mod computer;
pub mod config;
pub mod error;
pub mod event;
pub mod permission;
pub mod plugin;
pub mod provider;
pub mod schedule;
pub mod server;
pub mod session;
pub mod subagent;
pub mod tools;
pub mod token;

pub use error::HarnessError;
pub type Result<T> = std::result::Result<T, HarnessError>;
