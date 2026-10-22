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
#     caches = [ "https://<a kernel cache you trust>" ];
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
    attrPrefix = "";
    pythonPackages = null;
    flakeOutput = "packages";
    skipValidation = true;
    hydraUrl = "https://hydra.lantian.pub";
    hydraJobset = "lantian/nix-cachyos-kernel";
    hydraJobPattern = "{jobset}/packages.{arch}.{pkg}";
    hydraRevInput = "flake";
    flakeRef = "github:xddxdd/nix-cachyos-kernel";
    # Hydra only reports build status; it is never a trust root. The upstream
    # kernel cache (attic.xuyh0120.win/lantian) was shut down after a server
    # compromise (xddxdd/nix-cachyos-kernel#110) and cache.garnix.io no longer
    # resolves. cache.nixos.org does not carry these kernels, so there is no
    # generic cache to default to: consumers must choose one they trust.
    caches = throw "presets.cachyos-kernel: set `caches` to a binary cache you trust for nix-cachyos-kernel; the preset ships none (the upstream lantian cache was compromised, see https://github.com/xddxdd/nix-cachyos-kernel/issues/110)";
  };
}
