# Ready-to-use pin configurations for common use cases.
# Presets provide the boilerplate (attrPrefix, Hydra config, flakeRef, etc.)
# — you supply inputName and packages.
#
# Usage:
#   cache-pin.pins.rocm = inputs.nix-cache-pin.presets.rocm // {
#     inputName = "nixpkgs-rocm";
#     packages = [ "torchWithRocm" "torchvision" "torchaudio" ];
#   };
#
#   cache-pin.pins.cachyos = inputs.nix-cache-pin.presets.cachyos-kernel // {
#     inputName = "nix-cachyos-kernel";
#     packages = [ "linux-cachyos-latest-lto-zen4" ];
#   };
{
  # --- nixpkgs CUDA ---

  cuda = {
    attrPrefix = "pkgsCuda";
    pythonPackages = "python3Packages";
    hydraJobPattern = "{jobset}/{fullAttrPrefix}.{pkg}.{arch}";
  };

  # --- nixpkgs ROCm ---

  rocm = {
    attrPrefix = "pkgsRocm";
    pythonPackages = "python3Packages";
    hydraJobPattern = "{jobset}/{fullAttrPrefix}.{pkg}.{arch}";
  };

  # --- CachyOS kernels (Lantian Hydra) ---

  cachyos-kernel = {
    attrPrefix = "packages";
    pythonPackages = null;
    skipValidation = true;
    hydraUrl = "https://hydra.lantian.pub";
    hydraJobset = "lantian/nix-cachyos-kernel";
    hydraJobPattern = "{jobset}/packages.{arch}.{pkg}";
    hydraRevInput = "flake";
    flakeRef = "github:xddxdd/nix-cachyos-kernel";
    caches = [
      "https://attic.xuyh0120.win/lantian"
      "https://cache.garnix.io"
      "https://cache.nixos.org"
    ];
  };
}
