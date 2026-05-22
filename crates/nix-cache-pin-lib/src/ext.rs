use crate::error::Result;
use std::future::Future;

pub struct EvalAttrRequest<'a> {
    pub flake_ref: &'a str,
    pub rev: &'a str,
    pub arch: &'a str,
    pub flake_output: &'a str,
    pub attr_prefix: &'a str,
    pub pkg: &'a str,
    pub attr: &'a str,
}

/// Trait abstracting external commands (nix eval, gh api, nix flake lock)
/// to enable testing without spawning real processes.
pub trait ExternalCommands: Send + Sync {
    /// Evaluate the nix store path for a package at a given revision.
    fn eval_store_path(
        &self,
        flake_ref: &str,
        rev: &str,
        arch: &str,
        flake_output: &str,
        attr_prefix: &str,
        pkg: &str,
    ) -> impl Future<Output = Result<String>> + Send;

    /// Evaluate an arbitrary package attribute at a given revision.
    fn eval_attr_value(
        &self,
        request: EvalAttrRequest<'_>,
    ) -> impl Future<Output = Result<String>> + Send;

    /// List recent commits from a GitHub repo.
    fn list_commits(
        &self,
        owner_repo: &str,
        branch: &str,
        depth: usize,
    ) -> impl Future<Output = Result<Vec<String>>> + Send;

    /// Run `nix flake lock --update-input <input_name>`.
    fn run_flake_lock(&self, input_name: &str) -> impl Future<Output = Result<()>> + Send;
}

/// Real implementation that shells out to nix/gh.
pub struct RealCommands;

impl ExternalCommands for RealCommands {
    async fn eval_store_path(
        &self,
        flake_ref: &str,
        rev: &str,
        arch: &str,
        flake_output: &str,
        attr_prefix: &str,
        pkg: &str,
    ) -> Result<String> {
        crate::narinfo::eval_store_path(flake_ref, rev, arch, flake_output, attr_prefix, pkg).await
    }

    async fn eval_attr_value(&self, request: EvalAttrRequest<'_>) -> Result<String> {
        crate::narinfo::eval_attr_value(
            request.flake_ref,
            request.rev,
            request.arch,
            request.flake_output,
            request.attr_prefix,
            request.pkg,
            request.attr,
        )
        .await
    }

    async fn list_commits(
        &self,
        owner_repo: &str,
        branch: &str,
        depth: usize,
    ) -> Result<Vec<String>> {
        crate::github::list_commits(owner_repo, branch, depth).await
    }

    async fn run_flake_lock(&self, input_name: &str) -> Result<()> {
        crate::flake_update::run_flake_lock(input_name).await
    }
}
