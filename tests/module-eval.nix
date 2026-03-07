# Tests that package attribute paths used by cache-pin presets and configurations
# actually exist in nixpkgs, and that presets have the expected structure.
{
  lib,
  pkgs,
  ...
}: let
  presets = import ../nix/presets.nix;

  # Mirrors the validation logic from flake-module.nix
  assertPackagesExist = testName: attrPrefix: packages: let
    prefix = lib.attrByPath (lib.splitString "." attrPrefix) null pkgs;
    missing =
      builtins.filter (
        pkg: let
          parts = lib.splitString "." pkg;
        in
          lib.attrByPath parts null prefix == null
      )
      packages;
  in
    if prefix == null
    then throw "Test '${testName}': attrPrefix '${attrPrefix}' not found in nixpkgs"
    else if missing != []
    then throw "Test '${testName}': packages not found under '${attrPrefix}': ${lib.concatStringsSep ", " missing}"
    else
      pkgs.runCommand "cache-pin-test-${testName}" {} ''
        echo "cache-pin test '${testName}' passed: all packages exist under '${attrPrefix}'"
        echo "  packages: ${lib.concatStringsSep ", " packages}"
        touch $out
      '';

  # Validate a preset's attrPrefix resolves in nixpkgs
  assertPresetPrefix = name: preset: let
    prefixParts =
      lib.splitString "." preset.attrPrefix
      ++ lib.optionals (preset.pythonPackages or null != null) [preset.pythonPackages];
    fullPrefix = lib.concatStringsSep "." prefixParts;
    prefix = lib.attrByPath prefixParts null pkgs;
  in
    if prefix == null
    then throw "Preset '${name}': attrPrefix '${fullPrefix}' not found in nixpkgs"
    else
      pkgs.runCommand "cache-pin-test-preset-${name}" {} ''
        echo "Preset '${name}': attrPrefix '${fullPrefix}' resolves in nixpkgs"
        touch $out
      '';
in {
  # --- Preset validation ---
  # Presets don't include packages (users supply those), so we validate
  # that the attrPrefix they specify actually exists in nixpkgs.

  preset-cuda = assertPresetPrefix "cuda" presets.cuda;
  preset-rocm = assertPresetPrefix "rocm" presets.rocm;

  # CachyOS preset uses skipValidation (non-nixpkgs), so we verify
  # the preset attrset has the expected fields.
  preset-cachyos-kernel = let
    p = presets.cachyos-kernel;
    hasFields =
      p ? hydraUrl
      && p ? hydraJobset
      && p ? hydraJobPattern
      && p ? hydraRevInput
      && p ? flakeRef
      && p ? skipValidation
      && p.skipValidation == true;
  in
    pkgs.runCommand "cache-pin-test-preset-cachyos-kernel" {} ''
      ${
        if hasFields
        then ''echo "CachyOS kernel preset has all expected fields"''
        else ''echo "FAIL: CachyOS kernel preset missing fields" && exit 1''
      }
      touch $out
    '';

  # --- Attribute path validation (common package sets users might pin) ---

  cuda-toolkit = assertPackagesExist "cuda-toolkit" "cudaPackages" [
    "cudatoolkit"
    "cudnn"
    "nccl"
    "tensorrt"
  ];

  cuda-python-torch = assertPackagesExist "cuda-python-torch" "python3Packages" [
    "torchWithCuda"
    "torchvision"
    "torchaudio"
  ];

  rocm-core = assertPackagesExist "rocm-core" "rocmPackages" [
    "clr"
    "rocblas"
    "hipblas"
    "miopen"
    "rocm-smi"
  ];

  rocm-python-torch = assertPackagesExist "rocm-python-torch" "python3Packages" [
    "torchWithRocm"
    "torchvision"
    "torchaudio"
  ];

  rocm-llvm = assertPackagesExist "rocm-llvm" "rocmPackages" [
    "llvm.clang"
  ];

  desktop-apps = assertPackagesExist "desktop-apps" "pkgs" [
    "blender"
    "inkscape"
    "obs-studio"
  ];
}
