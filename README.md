# nix-cache-pin

<!-- simit:badges:start -->

![CI](https://img.shields.io/badge/CI-managed-2088ff) [![Nix](https://img.shields.io/badge/Nix-managed-5277c3)](flake.nix) [![crates.io](https://img.shields.io/badge/crates.io-ready-f46623)](https://crates.io/crates/cache-pin)

<!-- simit:badges:end -->

A [flake-parts](https://flake.parts) module that automatically pins your nixpkgs
flake inputs to revisions where **all** your specified packages have binary cache hits.

Stop waiting for builds. If Hydra (or your binary cache) has already built it,
`nix-cache-pin` finds the right revision for you.

## Problem

Nixpkgs-unstable moves fast. When you update your flake lock, some packages
(especially from overlay-heavy attribute sets like `pkgsRocm` or `pkgsCuda`)
may not yet have cache hits, forcing expensive local builds.

## Solution

Declare which packages you need cached, and `nix-cache-pin` will:

1. **Query Hydra** for recent nixpkgs evaluations
2. **Verify via narinfo** that packages are actually in the binary cache
3. **Fall back to scanning** GitHub commits if packages aren't on Hydra
4. Update your `flake.nix` input pin to the most recent fully-cached revision
5. Watch optional `wishPackages` and stop when one is ready to promote
6. Merge pin requirements that target the same flake input into one search

## Quick start

Add `nix-cache-pin` as a flake input and import the module:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    nix-cache-pin.url = "codeberg:caniko/nix-cache-pin";

    # Inputs that will be pinned to cached revisions
    nixpkgs-rocm.url = "github:NixOS/nixpkgs/<some-rev>";
    nixpkgs-cuda.url = "github:NixOS/nixpkgs/<some-rev>";
  };

  outputs = inputs:
    inputs.flake-parts.lib.mkFlake {inherit inputs;} {
      imports = [inputs.nix-cache-pin.flakeModules.default];

      systems = ["x86_64-linux"];

      cache-pin = {
        nixpkgs = inputs.nixpkgs.legacyPackages.x86_64-linux;

        pins.rocm = {
          packages = ["torchWithRocm" "torchvision"];
          wishPackages = ["obs-studio-plugins.obs-backgroundremoval"];
          inputName = "nixpkgs-rocm";
          attrPrefix = "pkgsRocm";
          pythonPackages = "python313Packages";
        };

        pins.cuda = {
          packages = ["cudatoolkit" "cudnn"];
          inputName = "nixpkgs-cuda";
          attrPrefix = "cudaPackages";
          pythonPackages = null;
        };
      };
    };
}
```

After setting this up, run the pin updater to find cached revisions:

```sh
nix run .#cache-pin         # update all pins
nix run .#cache-pin-rocm    # update a specific pin
nix run .#cache-pin-cuda    # update a specific pin
nix run .#cache-pin-update  # search and apply all pins as one transaction
```

## Standalone CLI tools

The project also provides standalone CLI tools that can be used independently:

```sh
# Check if store paths are in the binary cache
narinfo-check /nix/store/abc...-hello --cache https://cache.nixos.org
narinfo-check --config pin.json --rev <rev> --json

# Query Hydra for build status
hydra-query --config pin.json
hydra-query --hydra-url https://hydra.nixos.org --job nixpkgs/trunk/blender.x86_64-linux --json

# Evaluate nix store paths
nix-eval-store-path --flake-ref github:NixOS/nixpkgs --rev <rev> --attr blender
nix-eval-store-path --config pin.json --rev <rev> --json
```

## Presets

Ready-to-use configurations for common use cases:

```nix
cache-pin.pins.rocm = inputs.nix-cache-pin.presets.rocm // {
  inputName = "nixpkgs-rocm";
  packages = ["torchWithRocm" "torchvision" "torchaudio"];
};

cache-pin.pins.cachyos = inputs.nix-cache-pin.presets.cachyos-kernel // {
  inputName = "nix-cachyos-kernel";
  packages = ["linux-cachyos-latest-lto-zen4"];
};
```

Available presets: `rocm`, `cuda`, `cachyos-kernel`.

## Embedding in your own CLI

For downstream CLIs that wrap `nix-cache-pin`, the module exposes a pure-data
flake output enumerating the configured pin set:

```sh
nix eval --json .#cachePinMeta
# {"schemaVersion": 1, "pins": {"rocm": {"inputName": "nixpkgs-rocm", ...}, ...}}
```

Reading `cachePinMeta` does not trigger validation, so you can list pins even
when validation would otherwise throw. Use it to:

- Build per-pin subcommands or completions dynamically (no hardcoded names).
- Render help text that reflects whatever pins the user configured.
- Drive repo-doctor / CI freshness checks.

`schemaVersion` is the public contract — bumps signal a breaking change.

## Pin options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `packages` | `[str]` | *required* | Package attr paths relative to `attrPrefix` |
| `wishPackages` | `[str]` | `[]` | Packages to watch and promote to `packages` once built |
| `consumerFlakeRef` | `str?` | `null` | Flake whose consumer-specific targets should be evaluated |
| `consumerTargets` | `{ name = target; }` | `{}` | Map package names to derivation paths in `consumerFlakeRef` |
| `inputName` | `str` | *required* | Flake input name to update in `flake.nix` |
| `attrPrefix` | `str` | *required* | Top-level nixpkgs attr set (e.g. `pkgsRocm`) |
| `pythonPackages` | `str?` | `"pythonPackages"` | Python package set; set to `null` for non-Python packages |
| `caches` | `[str]` | `["https://cache.nixos.org"]` | Binary cache URLs to check |
| `hydraJobset` | `str` | `"nixpkgs/trunk"` | Hydra jobset to query |
| `hydraUrl` | `str` | `"https://hydra.nixos.org"` | Hydra instance URL |
| `hydraJobPattern` | `str` | `"{jobset}/{pkg}.{arch}"` | URL template for Hydra job lookups |
| `hydraRevInput` | `str` | `"nixpkgs"` | How to extract rev from Hydra evals |
| `flakeRef` | `str` | `"github:NixOS/nixpkgs"` | Flake reference (without revision) |
| `flakeOutput` | `str` | `"legacyPackages"` | Flake output attribute for eval |
| `arch` | `str?` | current system | System architecture |
| `depth` | `int` | `15` | Number of commits/evals to scan |
| `branch` | `str` | `"nixpkgs-unstable"` | Git branch for narinfo fallback |
| `branchFallbacks` | `[str]` | `[]` | Branches tried in order when the primary branch has no complete cache hit |
| `skipValidation` | `bool` | `false` | Skip nixpkgs attr path validation |
| `failFast` | `bool` | `false` | Exit on first cache miss |
| `lockOnly` | `bool` | `false` | Update only `flake.lock`, transactionally, leaving the source URL unchanged |
| `versionConstraints` | `{ attr = { target?, taints?, versionAttr?; }; }` | `{}` | Per-package version gates |

Updates are fail-before-write and transactional across `flake.nix`,
`flake.lock`, and the derived `cache-pin.lock.json` manifest. The lock file is
the source of truth; the manifest records the selected revision and the
individual requirements that were merged for each input. It is safe to delete
and regenerate the manifest by running `cache-pin-update` again.

Pins with different `inputName` values are never merged, even when they point
at the same nixpkgs source (for example, independent ROCm and CUDA package
sets). Pins with the same input are merged only when their evaluator settings
are compatible; package, wish-package, consumer-target, and version requirements
are combined, while conflicting settings fail with an actionable error.

## Wish packages

Use `wishPackages` for package attributes you want to require eventually but
which are not currently built by Hydra or present in your configured caches:

```nix
cache-pin.pins.rocm = {
  packages = ["blender" "obs-studio"];
  wishPackages = ["obs-studio-plugins.obs-backgroundremoval"];
  inputName = "nixpkgs-rocm";
  attrPrefix = "pkgsRocm";
  pythonPackages = null;
};
```

Every update applies a promotion gate before changing the pin:

1. Query Hydra's cheap `latest-finished` endpoint for every wish in parallel.
2. If none are on Hydra, find the normal target revision.
3. Check every wish at that revision against all configured binary caches.
4. Abort with the package names if any wish is built.

Move the reported names from `wishPackages` to `packages` and rerun. Required
and wish lists must be disjoint, and both receive the same attribute-path
validation.

## Consumer-aware targets

When a package is selected through a consuming flake's host configuration,
evaluate that exact derivation instead of a bare nixpkgs attribute:

```nix
cache-pin.pins.aarch64 = {
  packages = ["rauthy" "kanidm"];
  consumerFlakeRef = ".";
  consumerTargets = {
    rauthy = "nixosConfigurations.thething-crossbow.config.services.rauthy.package";
    kanidm = "nixosConfigurations.thething-crossbow.config.services.kanidm.package";
  };
  inputName = "nixpkgs";
  attrPrefix = "pkgs";
  arch = "aarch64-linux";
  lockOnly = true;
};
```

Each candidate is checked with `nix eval --override-input`, so follows,
overlays, and host-specific package selection are included. `lockOnly` then
merges the candidate lock graph transactionally and leaves the branch URL in
`flake.nix` untouched.

## Version gates

`versionConstraints` lets a pin reject or target revisions by package version
after cache hits are checked:

```nix
cache-pin.pins.cachyos-zen4 = inputs.nix-cache-pin.presets.cachyos-kernel // {
  inputName = "nix-cachyos-kernel-zen4";
  packages = ["linux-cachyos-latest-lto-zen4"];
  versionConstraints."linux-cachyos-latest-lto-zen4" = {
    target = "< 7.0.8";
    taints = [">= 7.0.8"];
    versionAttr = "version";
  };
};
```

- `target` accepts only versions matching the constraint.
- `taints` rejects versions matching any listed constraint.
- `versionAttr` defaults to `version`.
- Supported operators: exact, `=`, `!=`, `<`, `<=`, `>`, `>=`, `~`, `^`,
  and comma-separated ranges.

## Requirements

Runtime dependencies (provided automatically via the generated apps):
- `nix`, `git`, `gh` (GitHub CLI)

## CI

Woodpecker CI on Codeberg runs `nix flake check` on every push and pull request to verify module evaluation and the test suite.

## License

MIT
