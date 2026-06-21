//! replaykit library surface — exposes the internals so integration tests,
//! fuzz targets, and embedders can drive the engine directly. The CLI binary
//! is a thin wrapper over the same modules.

pub mod ca;
pub mod cassette;
pub mod cli;
pub mod commands;
pub mod config;
pub mod dashboard;
pub mod divergence;
pub mod matcher;
pub mod metrics;
pub mod proxy;
pub mod util;
