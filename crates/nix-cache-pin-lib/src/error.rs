use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("nix eval failed for {package}: {stderr}")]
    NixEval { package: String, stderr: String },

    #[error("GitHub API failed: {0}")]
    GitHub(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("cache-pin target '{input_name}' cannot merge pins {pins}: {reason}")]
    PinMerge {
        input_name: String,
        pins: String,
        reason: String,
    },

    #[error("flake.nix error: {0}")]
    FlakeNix(String),

    #[error("flake.nix not found at {0}")]
    FlakeNixNotFound(PathBuf),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("fail-fast: cache miss at rev {rev}")]
    FailFast { rev: String },

    #[error(
        "wish package(s) are now built ({location}): {packages}; move them from wishPackages to packages"
    )]
    WishPackagesBuilt { location: String, packages: String },

    #[error(
        "revision policy refused candidate {candidate} for current {current}: {relation}; use an explicit downgrade override for recovery"
    )]
    RevisionPolicy {
        current: String,
        candidate: String,
        relation: String,
    },

    #[error(
        "revision ordering is unknown for candidate {candidate} against current {current}; refusing to update without proof"
    )]
    RevisionOrderUnknown { current: String, candidate: String },

    #[error("{task} task failed: {source}")]
    TaskJoin {
        task: &'static str,
        source: tokio::task::JoinError,
    },
}

pub type Result<T> = std::result::Result<T, Error>;
