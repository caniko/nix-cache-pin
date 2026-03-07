# nix-cache-pin

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

1. **Query Hydra** or **probe binary caches** (via narinfo) for recent nixpkgs revisions
2. Find the most recent revision where **every** listed package is cached
3. Update your `flake.nix` input pin to that revision

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
          input = inputs.nixpkgs-rocm;
          packages = ["blender" "inkscape" "obs-studio"];
          inputName = "nixpkgs-rocm";
          attrPrefix = "pkgsRocm";
          strategy = "hydra";
        };

        pins.cuda = {
          input = inputs.nixpkgs-cuda;
          packages = ["cudatoolkit" "cudnn"];
          inputName = "nixpkgs-cuda";
          attrPrefix = "cudaPackages";
          strategy = "narinfo";
        };
      };

      # cachedRocmPackages and cachedCudaPackages are automatically
      # available as perSystem args when `input` is set
      perSystem = {cachedRocmPackages, cachedCudaPackages, ...}: {
        devShells.default = cachedRocmPackages.mkShell {
          packages = [
            cachedRocmPackages.blender
            cachedRocmPackages.obs-studio
            cachedCudaPackages.cudatoolkit
          ];
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
nix run .#cache-pin-update  # update all pins, then run `nix flake update`
```

This rewrites the pinned inputs in your `flake.nix` to point at the latest
nixpkgs revisions where all listed packages have cache hits. The pinned package
sets are then available anywhere — NixOS modules, home-manager, devShells, etc.

### NixOS configuration example

```nix
# In a NixOS module that receives cachedRocmPackages via specialArgs:
{cachedRocmPackages, ...}: {
  environment.systemPackages = [
    cachedRocmPackages.blender
    cachedRocmPackages.obs-studio
  ];
}
```

## Strategies

| Strategy | Use when | How it works |
|----------|----------|-------------|
| `hydra` | Packages are built by Hydra (e.g. `pkgsRocm`) | Queries Hydra's API for the latest eval where all packages succeeded |
| `narinfo` | Packages are NOT on Hydra (e.g. unfree `pkgsCuda`) | Evaluates store paths locally, then checks binary caches via HTTP HEAD on `.narinfo` |

## Pin options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `input` | flake input | `null` | The pinned nixpkgs flake input; when set, exposes packages as a `perSystem` arg |
| `exposedAs` | `str` | `cached<Name>Packages` | Name of the `perSystem` arg (e.g. pin `rocm` becomes `cachedRocmPackages`) |
| `packages` | `[str]` | *required* | Package attr paths relative to `attrPrefix` |
| `inputName` | `str` | *required* | Flake input name to update in `flake.nix` |
| `attrPrefix` | `str` | *required* | Top-level nixpkgs attr set (e.g. `pkgsRocm`) |
| `strategy` | `enum` | *required* | `"hydra"` or `"narinfo"` |
| `caches` | `[str]` | `["https://cache.nixos.org"]` | Binary cache URLs (narinfo only) |
| `hydraJobset` | `str` | `"nixpkgs/unstable"` | Hydra jobset to query (hydra only) |
| `arch` | `str` | current system | System architecture |
| `depth` | `int` | `15` | Number of recent commits to scan (narinfo only) |
| `branch` | `str` | `"nixpkgs-unstable"` | Git branch to scan (narinfo only) |
| `nixpkgsRepo` | `str` | `"NixOS/nixpkgs"` | GitHub repo in `owner/repo` format |

## Requirements

Runtime dependencies (provided automatically via the generated scripts):
- [Nushell](https://www.nushell.sh/)
- `nix`, `git`, `gh` (GitHub CLI), `curl`

## License

MIT
