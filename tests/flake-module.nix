# Integration tests for the flake-parts module.
# Verifies the module evaluates without errors and produces correct outputs.
{
  lib,
  pkgs,
  flake-parts-lib,
}: let
  # Mock cachePinSelf for tests — tests only validate module evaluation,
  # not actual binary execution, so the package doesn't need to be real.
  mockCachePinSelf = {
    packages = lib.genAttrs ["x86_64-linux" "aarch64-linux"] (_system: {
      all-binaries = pkgs.emptyDirectory;
    });
  };

  # Evaluate a minimal flake-parts configuration with cache-pin
  evalModule = cachePinConfig:
    flake-parts-lib.evalFlakeModule {
      inputs.self = {inputs = {};};
    } {
      imports = [
        (import ../nix/module.nix {cachePinSelf = mockCachePinSelf;})
      ];
      systems = ["x86_64-linux"];
      cache-pin = cachePinConfig;
    };

  system = pkgs.stdenv.hostPlatform.system;

  # Test: module evaluates with minimal config
  test-minimal-config = let
    evaluated = evalModule {
      nixpkgs = pkgs;
      pins = {};
    };
  in
    pkgs.runCommand "cache-pin-test-minimal-config" {} ''
      echo "Module evaluated successfully with empty pins"
      ${lib.optionalString (evaluated ? config) "echo 'config attribute exists'"}
      touch $out
    '';

  # Test: module evaluates with a basic pin (non-python packages)
  test-basic-pin = let
    evaluated = evalModule {
      nixpkgs = pkgs;
      pins.rocm = {
        packages = ["blender" "inkscape"];
        inputName = "nixpkgs-rocm";
        attrPrefix = "pkgsRocm";
        pythonPackages = null;
      };
    };
    perSystemConfig = evaluated.config.perSystem system;
  in
    pkgs.runCommand "cache-pin-test-basic-pin" {} ''
      ${
        if perSystemConfig.apps ? cache-pin-rocm
        then ''echo "cache-pin-rocm app exists"''
        else ''echo "FAIL: cache-pin-rocm app missing" && exit 1''
      }
      touch $out
    '';

  # Test: module evaluates with custom caches
  test-custom-caches = let
    evaluated = evalModule {
      nixpkgs = pkgs;
      pins.cuda = {
        packages = ["blender"];
        inputName = "nixpkgs-cuda";
        attrPrefix = "pkgsCuda";
        pythonPackages = null;
        caches = ["https://cache.nixos.org" "https://nix-community.cachix.org"];
      };
    };
    perSystemConfig = evaluated.config.perSystem system;
  in
    pkgs.runCommand "cache-pin-test-custom-caches" {} ''
      ${
        if perSystemConfig.apps ? cache-pin-cuda
        then ''echo "cache-pin-cuda app exists"''
        else ''echo "FAIL: cache-pin-cuda app missing" && exit 1''
      }
      touch $out
    '';

  # Test: aggregate app exists when pins are defined
  test-aggregate-apps = let
    evaluated = evalModule {
      nixpkgs = pkgs;
      pins.rocm = {
        packages = ["blender"];
        inputName = "nixpkgs-rocm";
        attrPrefix = "pkgsRocm";
        pythonPackages = null;
      };
    };
    perSystemConfig = evaluated.config.perSystem system;
  in
    pkgs.runCommand "cache-pin-test-aggregate-apps" {} ''
      ${
        if perSystemConfig.apps ? cache-pin
        then ''echo "cache-pin aggregate app exists"''
        else ''echo "FAIL: cache-pin aggregate app missing" && exit 1''
      }
      ${
        if perSystemConfig.apps ? cache-pin-update
        then ''echo "cache-pin-update app exists"''
        else ''echo "FAIL: cache-pin-update app missing" && exit 1''
      }
      touch $out
    '';

  # Test: app names are correctly prefixed with cache-pin-
  test-app-name-prefix = let
    evaluated = evalModule {
      nixpkgs = pkgs;
      pins.mypin = {
        packages = ["blender"];
        inputName = "nixpkgs-mypin";
        attrPrefix = "pkgsRocm";
        pythonPackages = null;
      };
    };
    perSystemConfig = evaluated.config.perSystem system;
  in
    pkgs.runCommand "cache-pin-test-app-name-prefix" {} ''
      ${
        if perSystemConfig.apps ? cache-pin-mypin
        then ''echo "App correctly named cache-pin-mypin"''
        else ''echo "FAIL: expected cache-pin-mypin, got: ${builtins.concatStringsSep ", " (builtins.attrNames perSystemConfig.apps)}" && exit 1''
      }
      ${
        if perSystemConfig.apps ? mypin
        then ''echo "FAIL: unprefixed app name 'mypin' should not exist" && exit 1''
        else ''echo "No unprefixed app name (correct)"''
      }
      touch $out
    '';

  # Test: invalid package triggers error
  # Evaluate the app program path to trigger validation (assert validated)
  test-invalid-package = let
    evaluated = evalModule {
      nixpkgs = pkgs;
      pins.test = {
        packages = ["this-package-does-not-exist-xyz"];
        inputName = "nixpkgs-test";
        attrPrefix = "pkgsRocm";
        pythonPackages = null;
      };
    };
    didThrow = builtins.tryEval (
      (evaluated.config.perSystem system).apps.cache-pin-test.program
    );
  in
    pkgs.runCommand "cache-pin-test-invalid-package" {} ''
      ${
        if !didThrow.success
        then ''echo "Correctly threw on invalid package"''
        else ''echo "FAIL: should have thrown on invalid package" && exit 1''
      }
      touch $out
    '';

  # Test: invalid attrPrefix triggers error
  test-invalid-prefix = let
    evaluated = evalModule {
      nixpkgs = pkgs;
      pins.test = {
        packages = ["blender"];
        inputName = "nixpkgs-test";
        attrPrefix = "pkgsDoesNotExist";
        pythonPackages = null;
      };
    };
    didThrow = builtins.tryEval (
      (evaluated.config.perSystem system).apps.cache-pin-test.program
    );
  in
    pkgs.runCommand "cache-pin-test-invalid-prefix" {} ''
      ${
        if !didThrow.success
        then ''echo "Correctly threw on invalid attrPrefix"''
        else ''echo "FAIL: should have thrown on invalid attrPrefix" && exit 1''
      }
      touch $out
    '';

  # Test: multiple pins coexist
  test-multiple-pins = let
    evaluated = evalModule {
      nixpkgs = pkgs;
      pins.rocm = {
        packages = ["blender"];
        inputName = "nixpkgs-rocm";
        attrPrefix = "pkgsRocm";
        pythonPackages = null;
      };
      pins.cuda = {
        packages = ["blender"];
        inputName = "nixpkgs-cuda";
        attrPrefix = "pkgsCuda";
        pythonPackages = null;
        caches = ["https://cache.nixos.org"];
      };
    };
    perSystemConfig = evaluated.config.perSystem system;
  in
    pkgs.runCommand "cache-pin-test-multiple-pins" {} ''
      ${
        if perSystemConfig.apps ? cache-pin-rocm
        then ''echo "cache-pin-rocm app exists"''
        else ''echo "FAIL: cache-pin-rocm app missing" && exit 1''
      }
      ${
        if perSystemConfig.apps ? cache-pin-cuda
        then ''echo "cache-pin-cuda app exists"''
        else ''echo "FAIL: cache-pin-cuda app missing" && exit 1''
      }
      ${
        if perSystemConfig.apps ? cache-pin
        then ''echo "aggregate cache-pin app exists"''
        else ''echo "FAIL: aggregate cache-pin app missing" && exit 1''
      }
      touch $out
    '';

  # Test: empty packages list evaluates without error
  test-empty-packages = let
    evaluated = evalModule {
      nixpkgs = pkgs;
      pins.empty = {
        packages = [];
        inputName = "nixpkgs-empty";
        attrPrefix = "pkgsRocm";
        pythonPackages = null;
      };
    };
    perSystemConfig = evaluated.config.perSystem system;
  in
    pkgs.runCommand "cache-pin-test-empty-packages" {} ''
      ${
        if perSystemConfig.apps ? cache-pin-empty
        then ''echo "cache-pin-empty app exists (empty packages list accepted)"''
        else ''echo "FAIL: cache-pin-empty app missing" && exit 1''
      }
      touch $out
    '';

  # Test: default option values land correctly in generated JSON config
  test-default-values-json = let
    evaluated = evalModule {
      nixpkgs = pkgs;
      pins.defaults = {
        packages = ["blender"];
        inputName = "nixpkgs-defaults";
        attrPrefix = "pkgsRocm";
        pythonPackages = null;
      };
    };
    pinConfig = evaluated.config.cache-pin.pins.defaults;
    json = builtins.fromJSON (builtins.toJSON {
      name = "defaults";
      inherit
        (pinConfig)
        packages
        inputName
        attrPrefix
        pythonPackages
        caches
        hydraJobset
        hydraUrl
        hydraJobPattern
        hydraRevInput
        depth
        branch
        flakeRef
        failFast
        ;
      arch = system;
    });
  in
    pkgs.runCommand "cache-pin-test-default-values-json" {} ''
      # Verify defaults
      ${
        if json.caches == ["https://cache.nixos.org"]
        then ''echo "caches default correct"''
        else ''echo "FAIL: caches default wrong" && exit 1''
      }
      ${
        if json.hydraJobset == "nixpkgs/trunk"
        then ''echo "hydraJobset default correct"''
        else ''echo "FAIL: hydraJobset default wrong" && exit 1''
      }
      ${
        if json.hydraUrl == "https://hydra.nixos.org"
        then ''echo "hydraUrl default correct"''
        else ''echo "FAIL: hydraUrl default wrong" && exit 1''
      }
      ${
        if json.hydraJobPattern == "{jobset}/{pkg}.{arch}"
        then ''echo "hydraJobPattern default correct"''
        else ''echo "FAIL: hydraJobPattern default wrong" && exit 1''
      }
      ${
        if json.hydraRevInput == "nixpkgs"
        then ''echo "hydraRevInput default correct"''
        else ''echo "FAIL: hydraRevInput default wrong" && exit 1''
      }
      ${
        if json.depth == 15
        then ''echo "depth default correct"''
        else ''echo "FAIL: depth default wrong" && exit 1''
      }
      ${
        if json.branch == "nixpkgs-unstable"
        then ''echo "branch default correct"''
        else ''echo "FAIL: branch default wrong" && exit 1''
      }
      ${
        if json.flakeRef == "github:NixOS/nixpkgs"
        then ''echo "flakeRef default correct"''
        else ''echo "FAIL: flakeRef default wrong" && exit 1''
      }
      ${
        if json.arch == system
        then ''echo "arch defaults to system"''
        else ''echo "FAIL: arch should default to system" && exit 1''
      }
      ${
        if json.failFast == false
        then ''echo "failFast default correct"''
        else ''echo "FAIL: failFast default wrong" && exit 1''
      }
      touch $out
    '';

  # Test: custom arch override flows through
  test-custom-arch = let
    evaluated = evalModule {
      nixpkgs = pkgs;
      pins.archtest = {
        packages = ["blender"];
        inputName = "nixpkgs-archtest";
        attrPrefix = "pkgsRocm";
        pythonPackages = null;
        arch = "aarch64-linux";
      };
    };
    pinConfig = evaluated.config.cache-pin.pins.archtest;
  in
    pkgs.runCommand "cache-pin-test-custom-arch" {} ''
      ${
        if pinConfig.arch == "aarch64-linux"
        then ''echo "Custom arch override accepted"''
        else ''echo "FAIL: arch should be aarch64-linux" && exit 1''
      }
      touch $out
    '';

  # Test: dotted attrPrefix (e.g. nested attr path) works for validation
  test-dotted-attr-prefix = let
    evaluated = evalModule {
      nixpkgs = pkgs;
      pins.nested = {
        packages = ["clr"];
        inputName = "nixpkgs-nested";
        attrPrefix = "rocmPackages";
        pythonPackages = null;
      };
    };
    perSystemConfig = evaluated.config.perSystem system;
  in
    pkgs.runCommand "cache-pin-test-dotted-attr-prefix" {} ''
      ${
        if perSystemConfig.apps ? cache-pin-nested
        then ''echo "Dotted attrPrefix validated and app created"''
        else ''echo "FAIL: cache-pin-nested app missing" && exit 1''
      }
      touch $out
    '';

  # Test: dotted package names (e.g. "llvm.clang") validate correctly
  test-dotted-package-name = let
    evaluated = evalModule {
      nixpkgs = pkgs;
      pins.dotpkg = {
        packages = ["llvm.clang"];
        inputName = "nixpkgs-dotpkg";
        attrPrefix = "rocmPackages";
        pythonPackages = null;
      };
    };
    perSystemConfig = evaluated.config.perSystem system;
  in
    pkgs.runCommand "cache-pin-test-dotted-package-name" {} ''
      ${
        if perSystemConfig.apps ? cache-pin-dotpkg
        then ''echo "Dotted package name llvm.clang validated"''
        else ''echo "FAIL: cache-pin-dotpkg app missing" && exit 1''
      }
      touch $out
    '';

  # Test: pin named "update" is rejected (reserved name collision)
  test-pin-name-collision = let
    evaluated = evalModule {
      nixpkgs = pkgs;
      pins.update = {
        packages = ["blender"];
        inputName = "nixpkgs-update";
        attrPrefix = "pkgsRocm";
        pythonPackages = null;
      };
    };
    didThrow = builtins.tryEval (
      (evaluated.config.perSystem system).apps.cache-pin-update.program
    );
  in
    pkgs.runCommand "cache-pin-test-pin-name-collision" {} ''
      ${
        if !didThrow.success
        then ''echo "Correctly rejected reserved pin name 'update'"''
        else ''echo "FAIL: should have thrown on reserved pin name" && exit 1''
      }
      touch $out
    '';

  # Test: default caches when not specified
  test-default-caches = let
    evaluated = evalModule {
      nixpkgs = pkgs;
      pins.defaultcache = {
        packages = ["blender"];
        inputName = "nixpkgs-defaultcache";
        attrPrefix = "pkgsCuda";
        pythonPackages = null;
      };
    };
    pinConfig = evaluated.config.cache-pin.pins.defaultcache;
  in
    pkgs.runCommand "cache-pin-test-default-caches" {} ''
      ${
        if pinConfig.caches == ["https://cache.nixos.org"]
        then ''echo "default caches correct"''
        else ''echo "FAIL: expected default cache URL" && exit 1''
      }
      touch $out
    '';

  # Test: pythonPackages with explicit version resolves torch packages
  test-python-packages-torch = let
    evaluated = evalModule {
      nixpkgs = pkgs;
      pins.torch = {
        packages = ["torchWithRocm" "torchvision"];
        inputName = "nixpkgs-rocm";
        attrPrefix = "pkgsRocm";
        pythonPackages = "python313Packages";
      };
    };
    perSystemConfig = evaluated.config.perSystem system;
  in
    pkgs.runCommand "cache-pin-test-python-packages-torch" {} ''
      ${
        if perSystemConfig.apps ? cache-pin-torch
        then ''echo "Torch pin with explicit pythonPackages validated"''
        else ''echo "FAIL: cache-pin-torch app missing" && exit 1''
      }
      touch $out
    '';

  # Test: default pythonPackages resolves to pythonPackages (latest python)
  test-python-packages-default = let
    evaluated = evalModule {
      nixpkgs = pkgs;
      pins.torchdefault = {
        packages = ["torch" "torchvision"];
        inputName = "nixpkgs-torch";
        attrPrefix = "pkgs";
      };
    };
    perSystemConfig = evaluated.config.perSystem system;
  in
    pkgs.runCommand "cache-pin-test-python-packages-default" {} ''
      ${
        if perSystemConfig.apps ? cache-pin-torchdefault
        then ''echo "Default pythonPackages resolves correctly"''
        else ''echo "FAIL: cache-pin-torchdefault app missing" && exit 1''
      }
      touch $out
    '';
  # Test: skipValidation allows arbitrary attrPrefix/packages
  test-skip-validation = let
    evaluated = evalModule {
      nixpkgs = pkgs;
      pins.custom = {
        packages = ["some-nonexistent-package"];
        inputName = "custom-input";
        attrPrefix = "doesNotExist";
        pythonPackages = null;
        skipValidation = true;
      };
    };
    perSystemConfig = evaluated.config.perSystem system;
  in
    pkgs.runCommand "cache-pin-test-skip-validation" {} ''
      ${
        if perSystemConfig.apps ? cache-pin-custom
        then ''echo "skipValidation allowed arbitrary packages"''
        else ''echo "FAIL: cache-pin-custom app missing" && exit 1''
      }
      touch $out
    '';

  # Test: custom Hydra source (like CachyOS) evaluates correctly
  test-custom-hydra-source = let
    evaluated = evalModule {
      nixpkgs = pkgs;
      pins.cachyos = {
        packages = ["linux-cachyos-latest-lto-zen4"];
        inputName = "nix-cachyos-kernel";
        attrPrefix = "packages";
        pythonPackages = null;
        skipValidation = true;
        hydraUrl = "https://hydra.lantian.pub";
        hydraJobset = "lantian/nix-cachyos-kernel";
        hydraJobPattern = "{jobset}/packages.{arch}.{pkg}";
        hydraRevInput = "flake";
        flakeRef = "github:xddxdd/nix-cachyos-kernel";
      };
    };
    perSystemConfig = evaluated.config.perSystem system;
    pinConfig = evaluated.config.cache-pin.pins.cachyos;
  in
    pkgs.runCommand "cache-pin-test-custom-hydra-source" {} ''
      ${
        if perSystemConfig.apps ? cache-pin-cachyos
        then ''echo "Custom Hydra source app exists"''
        else ''echo "FAIL: cache-pin-cachyos app missing" && exit 1''
      }
      ${
        if pinConfig.hydraUrl == "https://hydra.lantian.pub"
        then ''echo "hydraUrl override correct"''
        else ''echo "FAIL: hydraUrl override wrong" && exit 1''
      }
      ${
        if pinConfig.hydraRevInput == "flake"
        then ''echo "hydraRevInput override correct"''
        else ''echo "FAIL: hydraRevInput override wrong" && exit 1''
      }
      ${
        if pinConfig.flakeRef == "github:xddxdd/nix-cachyos-kernel"
        then ''echo "flakeRef override correct"''
        else ''echo "FAIL: flakeRef override wrong" && exit 1''
      }
      touch $out
    '';

  # Test: git+https flakeRef is accepted
  test-git-https-flake-ref = let
    evaluated = evalModule {
      nixpkgs = pkgs;
      pins.gitlab = {
        packages = ["some-pkg"];
        inputName = "gitlab-input";
        attrPrefix = "pkgs";
        pythonPackages = null;
        skipValidation = true;
        flakeRef = "git+https://gitlab.com/foo/bar";
      };
    };
    pinConfig = evaluated.config.cache-pin.pins.gitlab;
  in
    pkgs.runCommand "cache-pin-test-git-https-flake-ref" {} ''
      ${
        if pinConfig.flakeRef == "git+https://gitlab.com/foo/bar"
        then ''echo "git+https flakeRef accepted"''
        else ''echo "FAIL: flakeRef wrong" && exit 1''
      }
      touch $out
    '';

  # Test: mixed pins (nixpkgs + custom source) coexist
  test-mixed-sources = let
    evaluated = evalModule {
      nixpkgs = pkgs;
      pins.rocm = {
        packages = ["blender"];
        inputName = "nixpkgs-rocm";
        attrPrefix = "pkgsRocm";
        pythonPackages = null;
      };
      pins.cachyos = {
        packages = ["linux-cachyos"];
        inputName = "nix-cachyos-kernel";
        attrPrefix = "packages";
        pythonPackages = null;
        skipValidation = true;
        hydraUrl = "https://hydra.lantian.pub";
        hydraJobset = "lantian/nix-cachyos-kernel";
        hydraRevInput = "flake";
        flakeRef = "github:xddxdd/nix-cachyos-kernel";
      };
    };
    perSystemConfig = evaluated.config.perSystem system;
  in
    pkgs.runCommand "cache-pin-test-mixed-sources" {} ''
      ${
        if perSystemConfig.apps ? cache-pin-rocm
        then ''echo "nixpkgs pin exists"''
        else ''echo "FAIL: cache-pin-rocm missing" && exit 1''
      }
      ${
        if perSystemConfig.apps ? cache-pin-cachyos
        then ''echo "custom source pin exists"''
        else ''echo "FAIL: cache-pin-cachyos missing" && exit 1''
      }
      ${
        if perSystemConfig.apps ? cache-pin
        then ''echo "aggregate app exists"''
        else ''echo "FAIL: aggregate app missing" && exit 1''
      }
      touch $out
    '';
in {
  inherit
    test-minimal-config
    test-basic-pin
    test-custom-caches
    test-aggregate-apps
    test-app-name-prefix
    test-invalid-package
    test-invalid-prefix
    test-multiple-pins
    test-empty-packages
    test-default-values-json
    test-custom-arch
    test-dotted-attr-prefix
    test-dotted-package-name
    test-pin-name-collision
    test-default-caches
    test-python-packages-torch
    test-python-packages-default
    test-skip-validation
    test-custom-hydra-source
    test-git-https-flake-ref
    test-mixed-sources
    ;
}
