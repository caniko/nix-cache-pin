{
  lib,
  config,
  ...
}: let
  inherit
    (lib)
    mkOption
    types
    mapAttrs
    mapAttrsToList
    concatStringsSep
    ;

  pinSubmodule = types.submodule (
    {
      name,
      config,
      ...
    }: {
      options = {
        packages = mkOption {
          type = types.listOf types.str;
          description = ''
            Attribute paths (relative to `attrPrefix`) of packages that must
            all have cache hits for a revision to be selected.

            These are validated against the current nixpkgs at eval time.
          '';
          example = [
            "blender"
            "inkscape"
            "obs-studio"
          ];
        };

        inputName = mkOption {
          type = types.str;
          description = "Name of the flake input to update.";
          example = "nixpkgs-rocm";
        };

        attrPrefix = mkOption {
          type = types.str;
          description = ''
            The top-level attribute set in nixpkgs containing the packages.
            Used for local validation of package attribute paths.
            For cache lookups, `pythonPackages` is used directly (without this prefix)
            since Hydra builds python packages at the top level.
          '';
          example = "pkgsRocm";
        };

        pythonPackages = mkOption {
          type = types.nullOr types.str;
          default = "pythonPackages";
          description = ''
            Python package set to use (e.g. `python313Packages`).
            Packages are looked up under `attrPrefix.pythonPackages.<pkg>`.
            Defaults to `pythonPackages` (latest Python version in nixpkgs).
            Set to null to look up packages directly under `attrPrefix`.
          '';
          example = "python313Packages";
        };

        caches = mkOption {
          type = types.listOf types.str;
          default = ["https://cache.nixos.org"];
          description = ''
            Binary cache URLs to check for narinfo hits when verifying
            packages not found on Hydra.
          '';
          example = [
            "https://cache.nixos.org"
            "https://nix-community.cachix.org"
          ];
        };

        hydraJobset = mkOption {
          type = types.str;
          default = "nixpkgs/trunk";
          description = ''
            The Hydra jobset to query (e.g. `nixpkgs/trunk`).
          '';
        };

        arch = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = ''
            System architecture for Hydra jobs / nix eval.
            Defaults to the current system when null.
          '';
        };

        depth = mkOption {
          type = types.ints.positive;
          default = 15;
          description = ''
            Number of recent commits to scan when falling back to narinfo
            (when no packages are found on Hydra).
          '';
        };

        branch = mkOption {
          type = types.str;
          default = "nixpkgs-unstable";
          description = ''
            Git branch to scan for recent commits (narinfo fallback).
          '';
        };

        flakeRef = mkOption {
          type = types.str;
          default = "github:NixOS/nixpkgs";
          description = ''
            Flake reference for the input (without revision).
            The revision is appended automatically based on the scheme:
            - github:/gitlab:/sourcehut: → appends /<rev>
            - git+https:/git+ssh:/git+file: → appends ?rev=<rev>
          '';
          example = "github:xddxdd/nix-cachyos-kernel";
        };

        flakeOutput = mkOption {
          type = types.str;
          default = "legacyPackages";
          description = ''
            Top-level flake output attribute used for nix eval store path lookups.
            For nixpkgs-based inputs this is `legacyPackages`.
            For flakes that expose packages directly, use `packages`.
          '';
          example = "packages";
        };

        hydraUrl = mkOption {
          type = types.str;
          default = "https://hydra.nixos.org";
          description = "Base URL of the Hydra CI instance.";
          example = "https://hydra.lantian.pub";
        };

        hydraJobPattern = mkOption {
          type = types.str;
          default = "{jobset}/{pkg}.{arch}";
          description = ''
            URL path template for Hydra job lookups (appended to hydraUrl/job/).
            Available variables: {jobset}, {fullAttrPrefix}, {attrPrefix}, {arch}, {pkg}
          '';
          example = "{jobset}/packages.{arch}.{pkg}";
        };

        hydraRevInput = mkOption {
          type = types.str;
          default = "nixpkgs";
          description = ''
            How to extract the git revision from a Hydra evaluation.
            If set to "flake", parses the rev from the eval's flake URI.
            Otherwise, looks up the named input in jobsetevalinputs.<name>.revision.
          '';
          example = "flake";
        };

        skipValidation = mkOption {
          type = types.bool;
          default = false;
          description = ''
            Skip nixpkgs attribute path validation at eval time.
            Enable this for non-nixpkgs inputs where the packages can't
            be validated against the local nixpkgs.
          '';
        };

        failFast = mkOption {
          type = types.bool;
          default = false;
          description = ''
            If true, exit with an error immediately when any package is not
            found in the target cache store, instead of continuing to check
            remaining packages or revisions.
          '';
        };
      };
    }
  );
in {
  options.cache-pin = {
    pins = mkOption {
      type = types.attrsOf pinSubmodule;
      default = {};
      description = "Set of flake input pins to manage.";
      example = {
        rocm = {
          packages = [
            "torchWithRocm"
            "torchvision"
          ];
          inputName = "nixpkgs-rocm";
          attrPrefix = "pkgsRocm";
          pythonPackages = "python313Packages";
        };
      };
    };

    nixpkgs = mkOption {
      type = types.unspecified;
      description = ''
        The nixpkgs instance used for upfront validation of package attribute paths.
        Typically set to `inputs.nixpkgs.legacyPackages.''${system}` or similar.
      '';
    };
  };

  config = let
    cfg = config.cache-pin;

    # Pin names that would collide with the aggregate app names
    reservedNames = ["update"];

    # Validate that all declared packages exist under their attrPrefix in nixpkgs
    validatePin = name: pin: let
      prefixParts =
        lib.splitString "." pin.attrPrefix
        ++ lib.optionals (pin.pythonPackages != null) [pin.pythonPackages];
      fullPrefix = concatStringsSep "." prefixParts;
      prefix = lib.attrByPath prefixParts null cfg.nixpkgs;
      missing =
        builtins.filter (
          pkg: let
            parts = lib.splitString "." pkg;
          in
            lib.attrByPath parts null prefix == null
        )
        pin.packages;
    in
      if builtins.elem name reservedNames
      then throw "cache-pin.pins.${name}: '${name}' is a reserved pin name (conflicts with cache-pin-${name} aggregate app)"
      else if pin.skipValidation
      then true
      else if prefix == null
      then throw "cache-pin.pins.${name}: attribute path '${fullPrefix}' not found in nixpkgs"
      else if missing != []
      then throw "cache-pin.pins.${name}: packages not found under '${fullPrefix}': ${concatStringsSep ", " missing}"
      else true;

    # Generate JSON config for the nushell scripts (arch resolved per-system)
    pinToJson = system: name: pin:
      builtins.toJSON {
        inherit name;
        inherit
          (pin)
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
          flakeOutput
          failFast
          ;
        arch =
          if pin.arch != null
          then pin.arch
          else system;
      };

    # Force validation — seq ensures it runs before scriptDir is used
    validated = builtins.deepSeq (mapAttrs validatePin cfg.pins) true;
  in {
    perSystem = {
      pkgs,
      system,
      ...
    }: let
      scriptDir = assert validated; ../scripts;

      runtimePath = lib.makeBinPath (
        with pkgs; [
          nushell
          nix
          git
          gh
          curl
        ]
      );

      pinConfigs = mapAttrs (name: pin:
        pkgs.writeText "cache-pin-${name}.json" (pinToJson system name pin))
      cfg.pins;

      mkPinApp = name: pin:
        pkgs.writeShellScriptBin "cache-pin-${name}" ''
          export PATH="${runtimePath}:$PATH"
          exec ${pkgs.nushell}/bin/nu ${scriptDir}/cache-pin.nu --config ${pinConfigs.${name}} "$@"
        '';

      pinApps = mapAttrs mkPinApp cfg.pins;

      allApp = pkgs.writeShellScriptBin "cache-pin" ''
        set -euo pipefail
        export PATH="${runtimePath}:$PATH"
        ${concatStringsSep "\n" (
          mapAttrsToList (name: _pin: ''
            echo "=== Updating pin: ${name} ==="
            ${pkgs.nushell}/bin/nu ${scriptDir}/cache-pin.nu --config ${pinConfigs.${name}} "$@"
          '')
          cfg.pins
        )}
      '';
      updateAllApp = pkgs.writeShellScriptBin "cache-pin-update" ''
        set -euo pipefail
        export PATH="${runtimePath}:$PATH"
        ${concatStringsSep "\n" (
          mapAttrsToList (name: _pin: ''
            echo "=== Updating pin: ${name} ==="
            ${pkgs.nushell}/bin/nu ${scriptDir}/cache-pin.nu --config ${pinConfigs.${name}} "$@"
          '')
          cfg.pins
        )}
        echo "=== Running nix flake update ==="
        nix flake update
      '';
    in {
      apps =
        (lib.mapAttrs' (name: drv:
          lib.nameValuePair "cache-pin-${name}" {
            type = "app";
            program = "${drv}/bin/cache-pin-${name}";
          })
        pinApps)
        // {
          cache-pin = {
            type = "app";
            program = "${allApp}/bin/cache-pin";
          };
          cache-pin-update = {
            type = "app";
            program = "${updateAllApp}/bin/cache-pin-update";
          };
        };
    };
  };
}
