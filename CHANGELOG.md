# Changelog

## [Unreleased]

## [0.1.4] - 2026-10-11

### Fixed

- Refuse malformed narinfo for every cache availability check, including empty/HTML responses, mismatched store paths, invalid hashes/sizes, and malformed references. Retain parsing errors and continue to another configured cache when it supplies valid metadata. Fetch metadata with GET even when recursive closure verification is disabled.
- Treat HTTP 204/206 responses as unknown availability rather than cache hits. Preserve full referenced store paths so every closure node is checked against its exact name.
- Validate optional compression, signature and content-address fields using default Nix compatibility; do not assume experimental BLAKE3 or Git hashing is enabled. Bound metadata bodies to 1 MiB and a 30-second read deadline, retaining errors and alternate-cache fallback.

## [0.1.3] - 2026-10-11

### Fixed

- Search recent branch revisions before accepting the current pin, enforce branch eligibility for Hydra candidates, and deduplicate cache probes within an unchanged evaluation context.
- Paginate bounded Hydra history, order evaluation IDs newest first, and use the configured named input revision instead of a truncated producer revision. Preserve backend errors and producer build status.
- Verify every required target against the final combined staged lock before transactional apply. Roll back failed file installation and refuse incomplete selected input groups.
- Retain typed probe and per-cache HTTP evidence, report unknown availability separately from confirmed misses, and count version-rejected outputs as unaccepted.

### Added

- Embedded library version, archive VCS revision, and source fingerprint for consumer executable provenance.

## [0.1.2] - 2026-09-28

### Fixed

- Advance `flake.lock`'s original revision together with staged source URL edits, so a successful cache-pin transaction remains usable with `--no-update-lock-file`. Preserve original declarations for lock-only updates and reject mismatched resolved revisions.

## [0.1.1] - 2026-09-28

### Fixed

- Update pinned source URLs by input name, preserving other inputs that share the same URL or revision. This prevents CUDA and ROCm source pins from being swapped during a multi-input update.
- Reject stale or ambiguous named source edits before writing. Legacy unnamed file-update helpers now reject shared URLs rather than modifying the first match; the pure `replace_rev` helper leaves ambiguous content unchanged.

## [0.1.0] - 2026-09-28

### Added

- Prepare `nix-cache-pin-lib` and the `cache-pin`, `hydra-query`, `narinfo-check`, and `nix-eval-store-path` CLI crates for their first crates.io publication.
- Enforce configured version constraints on consumer-flake package targets, including checks against the current lock.

### Fixed

- Reuse valid hashes for unchanged Cargo Git sources while rejecting malformed source-pin sidecars before an update.
- Reject incomplete consumer target maps and conflicting package declarations at the library configuration boundary.
- Include license texts in published archives and update the locked rustls dependency to 0.23.45.
- Declare Git for sandboxed mutation tests so the Nix test gate exercises repository-lock discovery.
