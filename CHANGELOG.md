# Changelog

## [Unreleased]

## [0.1.0] - 2026-09-28

### Added

- Prepare `nix-cache-pin-lib` and the `cache-pin`, `hydra-query`, `narinfo-check`, and `nix-eval-store-path` CLI crates for their first crates.io publication.
- Enforce configured version constraints on consumer-flake package targets, including checks against the current lock.

### Fixed

- Reuse valid hashes for unchanged Cargo Git sources while rejecting malformed source-pin sidecars before an update.
- Reject incomplete consumer target maps and conflicting package declarations at the library configuration boundary.
- Include license texts in published archives and update the locked rustls dependency to 0.23.45.
- Declare Git for sandboxed mutation tests so the Nix test gate exercises repository-lock discovery.
