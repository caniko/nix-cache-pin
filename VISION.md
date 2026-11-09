# nix-cache-pin vision

nix-cache-pin is a flake-parts module that pins nixpkgs flake inputs to revisions
where **all** the packages you name already have binary cache hits. You declare
what you need cached; it queries Hydra for recent evaluations, verifies each
package through narinfo, falls back to scanning GitHub commits when Hydra does
not have it, and rewrites the flake input pin to the most recent revision that
is fully cached. The same library is embedded by tools that need to plan cache
pins themselves.

## Problem

Nixpkgs-unstable moves faster than Hydra finishes building it, and overlay-heavy
attribute sets such as `pkgsRocm` and `pkgsCuda` lag further behind. After a
`flake.lock` update, some packages have no cache hit, and the machine quietly
starts an expensive local build. Finding by hand the newest revision where every
required package is substitutable is tedious, and doing it once per input
multiplies the work.

## Principles

- **A cache hit is proven, not assumed.** A revision is accepted only when
  narinfo confirms the package is present; malformed narinfo is rejected before
  cache acceptance, and non-closure probes still validate metadata.
- **The final graph is the judge.** Pins are verified against the final flake
  graph and the exact required outputs of the consuming flake, not against an
  intermediate evaluation.
- **Failures are visible.** Terminal cache-search warnings are emitted only after
  all branches and the current-pin fallback have failed, and per-probe missing,
  unknown, and version-rejection evidence stays visible during the search.
- **Retries are bounded.** Hydra throttling and temporary unavailability are
  retried at most three times within a 60-second budget, honouring both forms of
  `Retry-After`.
- **Mutations are serialized.** Staged lock operations take an embedder-owned
  admission guard and are protected by Unix file locks, so concurrent runs cannot
  interleave edits.
- **No leaked processes.** Nix subprocesses are killed when their owning
  operation is cancelled.

## Scope

- the flake-parts module and generated pin app
- required packages, optional `wishPackages` promotion, and merging requirements
  that target the same flake input
- Hydra queries, narinfo checks, and GitHub commit-scan fallback
- source pin edits and final-graph cache verification
- a library (`nix-cache-pin-lib`) for embedders, plus the `cache-pin`,
  `hydra-query`, and `nix-eval-store-path` binaries

## Non-goals

It does not build the packages it pins for. A pin is only useful when the
configured binary cache already contains the outputs; the module's job is to find
that revision, not to make one buildable.

## Direction

Recent releases harden correctness and concurrency: final-graph verification,
bounded narinfo reads, rejection of malformed metadata, synchronized source pin
declarations, serialized mutations, and bounded Hydra retries. The current
unreleased work continues with an embedder-owned admission guard released before
final-graph evaluation and cache probes.

## Place in the fleet

Canix declares nix-cache-pin as a flake input (with dependent inputs following
it) and depends on `nix-cache-pin-lib` from its operations crate, where the
cache-pin planning is driven by `canix` commands.
