# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.0] - 2026-11-27

### Added

- `cachePinMeta` validation checks metadata before consumers use its pin
  specifications ([#42](https://github.com/caniko/nix-cache-pin/pull/42)).
- Batch pin searches keep pins for a shared input together and return no
  candidates to apply if any search fails
  ([#42](https://github.com/caniko/nix-cache-pin/pull/42)).

## [0.5.3] - 2026-11-24

### Changed

- Hydra requests now use adaptive pacing and bounded concurrency instead of a
  fixed one-second interval. By default, requests start at least 500 ms apart
  with up to four in flight, and slow down together when Hydra throttles them
  ([#39](https://github.com/caniko/nix-cache-pin/pull/39)).

### Security

- Updated `url` to 2.5.9 to address [RUSTSEC-2026-0592](https://rustsec.org/advisories/RUSTSEC-2026-0592.html).

## [0.4.2] - 2026-11-20

### Fixed

- A required package without Hydra evidence (an alias Hydra no longer builds,
  or a job it never built) no longer turns off the channel search for every
  package. A release with a successful required build and no failed or
  unfinished one is evaluated, and the evaluation checks the unknown targets.
  At most one such partially evidenced release is evaluated per search.
- A target the consumer builds itself (its path matches none of Hydra's
  outputs) that is not fully cached at any evaluated channel release ends the
  search instead of starting the commit walk, which could not find a cached
  build either. The walk still runs when any eligible release went unevaluated
  or the target was cached at one of them.

## [0.4.1] - 2026-11-15

### Fixed

- Channel job search asks for the job names Hydra publishes. A leading `pkgs`
  attribute component is dropped, so `attrPrefix = "pkgs"` looks up
  `nixpkgs.windmill.x86_64-linux` instead of the non-existent
  `nixpkgs.pkgs.windmill.x86_64-linux` that sent every channel lookup straight
  into the revision-history fallback. Other prefixes (`pkgsCuda`, `pkgsRocm`, a
  Python package set) are kept and stay unknown when the jobset lacks them.

### Changed

- Hydra requests are paced at one per second across all tasks and retries,
  below Hydra's published two per second with a burst of seven. The
  `nix-releases` bucket, which is not rate limited, is not paced. A throttled
  response honours Retry-After up to one minute per wait, backs off from two
  seconds otherwise, and gives up after six attempts or three minutes (a retry
  never takes a slot past that deadline), leaving unknown evidence rather than
  a failed build. A `/latest-finished` lookup also paces the redirect it
  follows.
- The channel evidence pass stops at the first required package that
  explicitly failed at every candidate release and reports that verdict without
  querying the remaining packages.

### Added

- `hydra::traffic()` reports the process's Hydra requests (every attempt and
  redirect hop), throttled responses and time spent waiting, for a consumer's
  run report.

## [0.4.0] - 2026-11-14

### Added

- `Error::LeaseTimeout` and the `lease-timeout` probe availability. An
  `ExternalCommands` implementation returns it when its evaluation admission
  is not granted in time. Probes, rev summaries and the never-cached warning
  then report "not checked" instead of a cache miss. Adding enum variants is
  breaking, so the next release is 0.4.0.

### Changed

- Nixpkgs channel branches (`nixos-unstable`, `nixos-unstable-small`,
  `nixos-YY.MM`, `nixos-YY.MM-small`, `nixpkgs-unstable`) now search Hydra
  channel releases instead of the newest commits. Releases newer than the
  current pin and on the configured branch tip are read from the public
  `nix-releases` bucket, gated on a finished, successful build for every
  required job in the channel's own jobset, and confirmed with one batched
  consumer evaluation. Every release in the window is resolved and validated,
  not only its endpoints. The previous commit walk remains the fallback for
  other branches, unreachable channel metadata and non-channel package sets.
- A required build that explicitly failed at every candidate release stops the
  channel search without evaluating any candidate release. The output names
  each package, its failing build URL and the releases covered; queued or
  unfinished jobs are reported as "not built yet" with the same verdict.
  Missing evidence — a release without `src-url` metadata, an eval outside the
  bounded build history, or a failed Hydra request — is reported as unknown
  and falls back to the revision history instead of being read as a failure.
- A consumer store path equal to Hydra's successful output skips the closure
  walk and checks only its root narinfo, but only when the evidence came from
  `https://hydra.nixos.org` and the root was answered by `https://cache.nixos.org`
  (a redirect elsewhere is attributed to the origin that answered); that pair
  carries Hydra's complete closures. Any other cache or Hydra keeps the
  configured closure check, and a different consumer path is reported as
  consumer-divergent.
- The commit-walk fallback checks the previous revision's missing targets
  first and rejects a revision as soon as one misses again at the same store
  path.
- `ExternalCommands::resolve_revision` resolves a release's short revision
  through the source host. Its default implementation uses `gh`; the new
  method is defaulted, so embedders keep compiling unchanged.
- Probe and final-graph evidence keep at most four cache attempts per package
  result, which bounds the JSON evidence consumers write without dropping
  availability, cache or error fields.

## [0.3.5] - 2026-11-12

### Fixed

- Release archives: the release workflow installs Nix with `cachix/install-nix-action`
  v31.11.1, fixing the 0.3.4 release, whose validation crashed the Nix daemon.

## [0.3.4] - 2026-11-12

### Fixed

- Release archives: the GitHub release now builds and publishes the signed
  cargo-binstall archives (the previous tag's release workflow failed).

## [0.3.3] - 2026-11-11

### Changed

- Release checksums are now minisign-signed: the release workflow writes
  `SHA256SUMS.txt`, signs it with the maintainer key published as
  `keys/minisign.pub`, and publishes `SHA256SUMS.txt.minisig` next to the
  cargo-binstall archives.

### Fixed

- The source-pin coverage check no longer reports a Git source as missing when
  several crates in `Cargo.lock` come from the same repository and revision.

## [0.3.2] - 2026-11-09

### Security

- Drop the compromised Lantian cache (`attic.xuyh0120.win/lantian`, see
  [xddxdd/nix-cachyos-kernel#110](https://github.com/xddxdd/nix-cachyos-kernel/issues/110))
  and the unresolvable `cache.garnix.io` from the `cachyos-kernel` preset.
  Pin configurations listing a cache on `attic.xuyh0120.win` are rejected.
- Refuse the compromised cache at every narinfo lookup, matched by parsed host, and refuse
  HTTP redirects to it. Other redirects follow reqwest's default policy.

### Fixed

- The generated source-pins check accepts lockfiles without Git dependencies.

### Changed

- The `cachyos-kernel` preset no longer ships a binary cache. Evaluation fails
  until consumers set `caches`; `cache.nixos.org` does not carry these kernels.
  Hydra build status still comes from `hydra.lantian.pub`.
- Evaluate a revision's store paths in one `nix eval` per flake instead of one per package.
  Consumer targets, locked-input targets and producer packages that share a flake and its flags
  are evaluated together, so the flake and what its attributes share (a NixOS configuration, a
  package set) are evaluated once. Each path resolves as its own evaluation would, including the
  installable `packages.<system>` prefixes; a batch that fails is retried package by package, so
  per-package results and errors are unchanged. `ExternalCommands::eval_store_paths` lets an
  embedder admit the whole batch once (the built-in implementation runs one evaluation at a
  time); its default, `ext::eval_each`, calls the existing per-package methods.

## [0.3.1] - 2026-10-24

### Fixed

- Keep the `cache-pin.lock.json` targets of inputs a run does not cover. A run selecting one
  pin's input group no longer drops every other input's recorded revision; an unreadable
  existing receipt is an error instead of being overwritten.
- Refuse lock-only updates of inputs declared at a fixed revision. Updating only the lock left
  `flake.lock` with an `original` revision that disagreed with the locked one; such pins must
  drop `lockOnly` so the declaration advances with the lock.

### Security

- Update the locked `tokio` to 1.51.6, addressing RUSTSEC-2026-0386.

## [0.3.0] - 2026-10-21

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

[Unreleased]: https://github.com/caniko/nix-cache-pin/compare/0.6.0...HEAD
[0.6.0]: https://github.com/caniko/nix-cache-pin/compare/0.5.3...0.6.0
[0.5.3]: https://github.com/caniko/nix-cache-pin/compare/0.4.2...0.5.3
[0.4.2]: https://github.com/caniko/nix-cache-pin/compare/0.4.1...0.4.2
[0.4.1]: https://github.com/caniko/nix-cache-pin/compare/0.4.0...0.4.1
[0.4.0]: https://github.com/caniko/nix-cache-pin/compare/0.3.5...0.4.0
[0.3.5]: https://github.com/caniko/nix-cache-pin/compare/0.3.4...0.3.5
[0.3.4]: https://github.com/caniko/nix-cache-pin/compare/0.3.3...0.3.4
[0.3.3]: https://github.com/caniko/nix-cache-pin/compare/0.3.2...0.3.3
[0.3.2]: https://github.com/caniko/nix-cache-pin/compare/0.3.1...0.3.2
[0.3.1]: https://github.com/caniko/nix-cache-pin/compare/0.3.0...0.3.1
[0.3.0]: https://github.com/caniko/nix-cache-pin/compare/0.2.3...0.3.0
