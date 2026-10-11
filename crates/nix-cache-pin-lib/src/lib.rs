#![deny(unused_must_use)]

pub mod config;
pub mod error;
pub mod ext;
pub mod final_graph;
pub mod flake_update;
pub mod flakeref;
pub mod github;
pub mod hydra;
pub mod manifest;
pub mod merge;
mod mutation;
pub mod narinfo;
pub mod orchestrate;
pub mod output;
pub mod plan;
pub mod runner;
pub mod source_pins;
pub mod transaction;
pub mod version;
/// Identity of the embedded engine, independent of the consumer's flake pin.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const SOURCE_REVISION: &str = env!("NIX_CACHE_PIN_SOURCE_REVISION");
pub const SOURCE_SHA256: &str = env!("NIX_CACHE_PIN_SOURCE_SHA256");
#[cfg(test)]
mod test_cache;
