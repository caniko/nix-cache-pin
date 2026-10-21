# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Wheel pins (`cache-pin.wheel-pins.<name>`, `cache-pin-wheel-pins`) pin prebuilt Python
  wheels, such as PyTorch ROCm/XPU builds, to exact files and sha256 hashes in a checked-in
  JSON sidecar.
  - Resolution uses PEP 440 specifiers and local version labels, CPython and platform tag
    selection, and the `Requires-Dist` closure with PEP 508 markers. Metadata comes from
    PEP 658, or from a remote wheel's `METADATA` read with HTTP range requests (ZIP64
    included).
  - Modes: `--update`, `--dry-run`, `--check-current`, `--plan`, and an offline
    `--verify-sidecar`.
  - Each pin gets an offline `cache-pin-wheel-pins-<name>-coverage` flake check, and
    `flake.wheelPinsMeta` lists the declarations.

### Security

- Update the locked `rustls-webpki` to 0.103.16 and `webpki-root-certs` to 1.0.10,
  addressing RUSTSEC-2026-0382 and RUSTSEC-2026-0383.

## [0.2.3] - 2026-10-14

### Security

- Update the locked Rustls dependency to 0.23.46, addressing
  RUSTSEC-2026-0346, RUSTSEC-2026-0347, RUSTSEC-2026-0348 and RUSTSEC-2026-0349.

### Fixed

- Detect stderr's terminal and color policy for library diagnostics, including
  when stdout is redirected for JSON.

### Changed

- Use Reqwest 0.13.5's Rustls backend with platform certificate verification,
  retaining system proxy detection, HTTP/2, charset, JSON, query and streaming
  features. Remove the native-TLS/OpenSSL dependency chain from default clients.
  TLS backend features remain Cargo-unified: embedders enabling native TLS on
  Reqwest can still change its default backend. Client injection remains supported.
- Keep HTTP-only runtimes usable without a CA store. Default client construction
  is fallible and retries with empty roots when platform initialization fails;
  HTTPS remains certificate-verified and fails closed without trusted roots.

## [0.2.2] - 2026-10-12

### Fixed

- Store exact copies of the existing MIT and Apache license texts as regular
  package files, so signed-source reconstruction does not depend on Git links.
  Retain the license terms, source admission policy, and original archive custody.
- Copy the static Pages assets without inheriting read-only Nix store modes,
  allowing the site derivation to write its canonical-domain marker.

## [0.2.1] - 2026-10-12

### Changed

- Reuse the MIT-licensed `console` styling backend already present through
  progress reporting, removing the MPL-2.0 `colored` dependency from the library
  and CLI graphs while retaining diagnostic wording and status colors.
- Qualify reqwest 0.13.5, hyper-util 0.1.21, and base64 0.23 together so runtime
  HTTP clients and strict narinfo signature decoding use one base64 generation.
  Retain the existing HTTP features, canonical decoding, cache acceptance policy, typed
  receipts, and Rust 1.88 minimum toolchain.
- Build release archives explicitly through the qualified Simit generator and
  verify them in the independently enrolled default-branch publisher before
  environment-only registry upload.

## [0.2.0] - 2026-10-12

### Changed

- Upgrade the progress renderer to indicatif 0.18, removing the unmaintained `number_prefix` dependency.
- Upgrade HTTP clients to reqwest 0.13 with explicit features preserving native TLS, system proxy detection, HTTP/2, charset decoding, JSON, query parameters and streaming.
- Declare package contents and docs.rs features for every workspace crate, including integration fixtures in the library archive. Cargo retains the workspace README and both license texts in each package.

### Migration

- Library callers passing their own HTTP client must use reqwest 0.13. Callers of `Output::spinner_in` must use indicatif 0.18's `MultiProgress`. These public dependency types require a new minor release; the CLI, pin policy and receipt formats retain their existing contracts.

## [0.1.5] - 2026-10-12

### Fixed

- Emit terminal cache-search warnings only after all branches and the current-pin fallback fail. Keep per-probe missing, unknown, and version-rejection evidence visible during search.
- Retry Hydra throttling and temporary unavailability at most three times within a 60-second budget, honoring both forms of `Retry-After`. Reuse complete revision mappings already returned by jobset history.
- Kill outstanding Nix subprocesses when their owning operation is cancelled.

### Added

- An embedder-owned admission guard for staged lock operations, released before final-graph evaluation and cache probes.

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
