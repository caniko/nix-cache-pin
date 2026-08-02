{cachePinSelf}: {
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
    escapeShellArg
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

        wishPackages = mkOption {
          type = types.listOf types.str;
          default = [];
          description = ''
            Package attribute paths that are not currently expected to have
            cache hits, but should be promoted to `packages` once built.

            Each update first checks Hydra for these packages, then checks the
            selected revision against `caches`. If any wish is built, the
            update aborts before changing the pin and names the packages to
            promote.
          '';
          example = ["obs-studio-plugins.obs-backgroundremoval"];
        };

        consumerFlakeRef = mkOption {
          type = types.nullOr types.str;
          default = null;
          description = ''
            Optional consuming flake reference used to evaluate package
            targets. When set, `consumerTargets` are evaluated with the
            candidate input overridden, so follows and consumer overlays are
            preserved.
          '';
          example = ".";
        };

        consumerTargets = mkOption {
          type = types.attrsOf types.str;
          default = {};
          description = ''
            Map from package names in `packages`/`wishPackages` to attribute
            paths in `consumerFlakeRef`. Each path must evaluate to a
            derivation. This is the preferred mode for packages consumed
            through a host or another flake.
          '';
          example = {
            rauthy = "cachePinTargets.aarch64.rauthy";
          };
        };

        requiredConsumerTargets = mkOption {
          type = types.attrsOf types.str;
          default = {};
          description = ''
            Map from unique labels to exact derivation paths in
            `consumerFlakeRef`. Every target is a required cache gate, but is
            not queried on Hydra or treated as a source-flake package attr.
          '';
          example = {
            host = "nixosConfigurations.thething.config.system.build.toplevel";
          };
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

        branchFallbacks = mkOption {
          type = types.listOf types.str;
          default = [];
          description = ''
            Additional branches to scan in order when the primary branch has
            no revision with cache hits for the complete package set.
          '';
          example = ["nixpkgs-unstable"];
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

        lockOnly = mkOption {
          type = types.bool;
          default = false;
          description = ''
            Update only the lock file through a transactional temporary lock.
            The flake input URL remains unchanged. This is useful when the
            lock file is the authoritative pin and the source URL is a branch.
          '';
        };

        verifyClosure = mkOption {
          type = types.bool;
          default = false;
          description = ''
            Verify the complete binary-cache closure of every selected store
            path by following `References` in narinfo responses. Enable this
            for deployment-critical package sets where a cached top-level
            narinfo is insufficient proof that activation will avoid builds.
          '';
        };

        versionConstraints = mkOption {
          type = types.attrsOf (types.submodule {
            options = {
              target = mkOption {
                type = types.nullOr types.str;
                default = null;
                description = ''
                  Version constraint that must match for this package at a
                  candidate revision.
                '';
                example = "< 7.0.8";
              };

              taints = mkOption {
                type = types.listOf types.str;
                default = [];
                description = ''
                  Version constraints that reject a candidate revision when
                  any one of them matches.
                '';
                example = [">= 7.0.8"];
              };

              versionAttr = mkOption {
                type = types.str;
                default = "version";
                description = ''
                  Package attribute to evaluate for version checks.
                '';
              };
            };
          });
          default = {};
          description = ''
            Per-package version gates. Keys are package attr paths relative to
            `attrPrefix`; values define target and taint constraints.
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

    source-pins = mkOption {
      type = types.attrsOf (types.submodule {
        options = {
          type = mkOption {
            type = types.enum ["cargo-git"];
            description = "Lock file format for source extraction.";
          };
          lockFile = mkOption {
            type = types.path;
            description = "Lock file to extract sources from.";
          };
          outputFile = mkOption {
            type = types.str;
            description = ''
              Relative path from flake root to the generated Nix sidecar.
              The sidecar maps source strings to narHashes and is checked in.
            '';
          };
        };
      });
      default = {};
      description = "Set of source pins to manage (e.g. Cargo.lock git deps).";
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
      allPackages = pin.packages ++ pin.wishPackages;
      overlap = builtins.filter (pkg: builtins.elem pkg pin.wishPackages) pin.packages;
      unknownConsumerTargets =
        builtins.filter (pkg: !(builtins.hasAttr pkg pin.consumerTargets)) allPackages;
      consumerTargetsWithoutPackages =
        builtins.filter (pkg: !(builtins.elem pkg allPackages)) (builtins.attrNames pin.consumerTargets);
      requiredTargetLabels = builtins.attrNames pin.requiredConsumerTargets;
      requiredTargetOverlap =
        builtins.filter (
          label:
            builtins.elem label allPackages
            || builtins.hasAttr label pin.consumerTargets
        )
        requiredTargetLabels;
      missing =
        builtins.filter (
          pkg: let
            parts = lib.splitString "." pkg;
          in
            lib.attrByPath parts null prefix == null
        )
        allPackages;
    in
      if builtins.elem name reservedNames
      then throw "cache-pin.pins.${name}: '${name}' is a reserved pin name (conflicts with cache-pin-${name} aggregate app)"
      else if overlap != []
      then throw "cache-pin.pins.${name}: packages and wishPackages overlap: ${concatStringsSep ", " overlap}"
      else if pin.consumerTargets != {} && pin.consumerFlakeRef == null
      then throw "cache-pin.pins.${name}: consumerTargets requires consumerFlakeRef"
      else if pin.requiredConsumerTargets != {} && pin.consumerFlakeRef == null
      then throw "cache-pin.pins.${name}: requiredConsumerTargets requires consumerFlakeRef"
      else if requiredTargetOverlap != []
      then throw "cache-pin.pins.${name}: requiredConsumerTargets labels overlap packages, wishPackages, or consumerTargets: ${concatStringsSep ", " requiredTargetOverlap}"
      else if pin.consumerTargets != {} && unknownConsumerTargets != []
      then throw "cache-pin.pins.${name}: consumer target missing for: ${concatStringsSep ", " unknownConsumerTargets}"
      else if consumerTargetsWithoutPackages != []
      then throw "cache-pin.pins.${name}: consumerTargets has untracked packages: ${concatStringsSep ", " consumerTargetsWithoutPackages}"
      else if pin.skipValidation
      then true
      else if prefix == null
      then throw "cache-pin.pins.${name}: attribute path '${fullPrefix}' not found in nixpkgs"
      else if missing != []
      then throw "cache-pin.pins.${name}: packages not found under '${fullPrefix}': ${concatStringsSep ", " missing}"
      else true;

    # Generate JSON config for cache-pin (arch resolved per-system)
    pinToJson = system: name: pin:
      builtins.toJSON {
        inherit name;
        inherit
          (pin)
          packages
          wishPackages
          consumerFlakeRef
          consumerTargets
          requiredConsumerTargets
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
          branchFallbacks
          flakeRef
          flakeOutput
          failFast
          lockOnly
          verifyClosure
          versionConstraints
          ;
        arch =
          if pin.arch != null
          then pin.arch
          else system;
      };

    # Force validation — seq ensures it runs before binaries are used
    validated = builtins.deepSeq (mapAttrs validatePin cfg.pins) true;

    # Pure-data view of the configured pin set, suitable for downstream CLIs to
    # enumerate pins via `nix eval --json .#cachePinMeta`. Does not depend on
    # any system, does not trigger validation, does not require a build.
    pinMeta =
      mapAttrs (_name: pin: {
        inherit
          (pin)
          packages
          wishPackages
          consumerFlakeRef
          consumerTargets
          requiredConsumerTargets
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
          branchFallbacks
          flakeRef
          flakeOutput
          skipValidation
          failFast
          lockOnly
          verifyClosure
          versionConstraints
          ;
        arch = pin.arch; # null if unset — consumer resolves to current system
      })
      cfg.pins;

    # --- source-pins: metadata for cargo git dep hash updater ---
    sourcePinMeta =
      mapAttrs (_name: pin: {
        type = pin.type;
        lockFile = builtins.toString pin.lockFile;
        outputFile = pin.outputFile;
      })
      cfg.source-pins;
  in {
    flake.cachePinMeta = {
      schemaVersion = 4;
      pins = pinMeta;
    };

    flake.sourcePinsMeta = {
      schemaVersion = 1;
      pins = sourcePinMeta;
    };

    perSystem = {
      pkgs,
      system,
      ...
    }: let
      cachePinBinaries = assert validated; cachePinSelf.packages.${system}.all-binaries;

      runtimePath = lib.makeBinPath (
        with pkgs;
          [
            nix
            git
            gh
          ]
          ++ [cachePinBinaries]
      );

      pinConfigs = mapAttrs (name: pin:
        pkgs.writeText "cache-pin-${name}.json" (pinToJson system name pin))
      cfg.pins;

      mkPinApp = name: pin: let
        groupNames = builtins.filter (other: (builtins.getAttr other cfg.pins).inputName == pin.inputName) (builtins.attrNames cfg.pins);
        groupArgs = concatStringsSep " " (map (other: "--config ${builtins.getAttr other pinConfigs}") groupNames);
      in
        pkgs.writeShellScriptBin "cache-pin-${name}" ''
          export PATH="${runtimePath}:$PATH"
          exec cache-pin ${groupArgs} "$@"
        '';

      pinApps = mapAttrs mkPinApp cfg.pins;

      allConfigArgs = concatStringsSep " " (
        mapAttrsToList (name: _: "--config ${pinConfigs.${name}}") cfg.pins
      );

      allApp = pkgs.writeShellScriptBin "cache-pin" ''
        set -euo pipefail
        export PATH="${runtimePath}:$PATH"
        exec cache-pin ${allConfigArgs} "$@"
      '';
      updateAllApp = pkgs.writeShellScriptBin "cache-pin-update" ''
        set -euo pipefail
        export PATH="${runtimePath}:$PATH"
        exec cache-pin ${allConfigArgs} --update "$@"
      '';

      # --- source-pins: cargo git dep hash updaters ---
      jqBin = "${pkgs.jq}/bin/jq";
      nixBin = "${pkgs.nix}/bin/nix";

      mkSourcePinUpdate = name: pin: let
        lockFileStorePath = builtins.toString pin.lockFile;
      in
        pkgs.writeShellScriptBin "cache-pin-source-pins-${name}" ''
          set -euo pipefail

          n="${escapeShellArg name}"
          output_rel="${escapeShellArg pin.outputFile}"
          lock="${escapeShellArg lockFileStorePath}"
          nix_bin="${nixBin}"

          echo "=== cache-pin source-pins: $n ==="
          echo "  Lock file:  $lock"

          repo_root="$(pwd)"
          output_file="$repo_root/$output_rel"
          echo "  Output:      $output_file"

          # --- Pre-check: skip if sidecar already covers all Cargo.lock sources ---
          if [ -f "$output_file" ]; then
          lock_keys=$(rg 'source = "git\+' "$lock" \
            | sed 's/.*source = "//;s/"$//' \
            | sed 's/%2F/\//g; s/%23/#/g; s/%3F/?/g; s/%3D/=/g; s/%26/\&/g' \
            | sort -u 2>/dev/null || true)
          sidecar_keys=$(rg '^\s+"git\+' "$output_file" \
            | sed 's/^\s*"//;s/" =.*//' \
            | sort -u 2>/dev/null || true)
            missing=$(comm -23 <(echo "$lock_keys") <(echo "$sidecar_keys") 2>/dev/null)
            if [ -z "$missing" ] && [ -n "$lock_keys" ]; then
              echo "  Sidecar is current — no prefetch needed"
              exit 0
            fi
          fi

          # --- Set up temp dir for parallel prefetch ---
          tmpdir=$(mktemp -d)
          cleanup() { rm -rf "$tmpdir"; }
          trap cleanup EXIT

          # --- Prefetch all git deps in parallel ---
          echo "  Fetching hashes for $(rg -c 'source = "git\+' "$lock" 2>/dev/null || echo 0) git deps..."

          while IFS= read -r src; do
                (
                  decoded=$(printf '%s' "$src" | sed 's/%/\\x/g' | xargs -0 printf '%b' 2>/dev/null || echo "$src")
                  key="$decoded"
                  url="''${decoded#git+}"

                  if echo "$url" | grep -q '?rev='; then
                    rev=$(echo "$url" | sed 's/.*?rev=\([^&#]*\).*/\1/')
                    base=$(echo "$url" | sed 's/?rev=[^&#]*//;s/#.*//')
                    fetch_url="''${base}?rev=''${rev}"
                  else
                    rev=$(echo "$url" | sed 's/.*#//')
                    base=$(echo "$url" | sed 's/#.*//')
                    fetch_url="''${base}?rev=''${rev}"
                  fi

                  echo "    Fetching: $fetch_url" >&2
                  hash=$($nix_bin flake prefetch "$fetch_url" 2>&1 | grep -oP "hash '\K[^']+") || {
                    echo "FAIL" > "$tmpdir/fail"
                    exit 1
                  }
                  echo "$key|$hash" > "$tmpdir/$(echo "$src" | sha1sum 2>/dev/null | cut -c1-16 || echo "$RANDOM")"
                  echo "    Got hash: $hash" >&2
                ) &
          done < <(
            rg 'source = "git\+' "$lock" \
              | sed 's/.*source = "//;s/"$//' \
              | sort -u
          )

          wait

          # Check for failures
          if [ -f "$tmpdir/fail" ]; then
            echo "ERROR: one or more prefetch jobs failed" >&2
            exit 1
          fi

          # --- Write sidecar ---
          > "$output_file.tmp"
          {
            echo "# Generated by nix-cache-pin source-pins — do not edit manually."
            echo "# Lock file:   $lock"
            echo "# Source pin:  $n"
            echo "# Updated:     $(date -u +%Y-%m-%dT%H:%M:%SZ)"
            echo "{"
            for f in "$tmpdir"/????????????????; do
              [ -f "$f" ] || continue
              IFS='|' read -r key hash < "$f"
              if [ -n "$key" ] && [ -n "$hash" ]; then
                short=$(echo "$key" | head -c 80)
                echo "  # $short"
                echo "  \"$key\" = \"$hash\";"
              fi
            done | sort
            echo "}"
          } > "$output_file.tmp"

          if [ -f "$output_file" ] && cmp -s "$output_file.tmp" "$output_file"; then
            echo "  No changes — $(basename "$output_file") is current"
            rm -f "$output_file.tmp"
          else
            mv "$output_file.tmp" "$output_file"
            echo "  Updated $(basename "$output_file")"
          fi
        '';

      sourcePinUpdateScripts = mapAttrs mkSourcePinUpdate cfg.source-pins;

      mkSourcePinCoverage = name: pin: let
        lockFilePath = pin.lockFile;
        # Derive flake root from lock file path (e.g. .../source/cli/Cargo.lock → .../source)
        lockFileStr = toString lockFilePath;
        flakeRoot = builtins.dirOf (builtins.dirOf lockFileStr);
        sidecarPath = builtins.path {
          path = "${flakeRoot}/${pin.outputFile}";
          name = "source-pins-sidecar-${name}";
        };
      in
        pkgs.runCommand "cache-pin-source-pins-${name}-coverage" {
          nativeBuildInputs = with pkgs; [diffutils gnused ripgrep];
          srcLockFile = lockFilePath;
          srcSidecar = sidecarPath;
        } ''
          set -euo pipefail

          rg 'source = "git\+' "$srcLockFile" \
            | sed 's/.*source = "//;s/"$//' \
            | sed 's|^git+https://codeberg.org/|git+ssh://git@codeberg.org/|' \
            | sed 's/%2F/\//g; s/%23/#/g; s/%3F/?/g; s/%3D/=/g; s/%26/\&/g' \
            | sort > "$TMPDIR/lock_sources"

          if [ -f "$srcSidecar" ]; then
            rg '^\s+"git\+' "$srcSidecar" \
              | sed 's/^\s*"//;s/" =.*//' \
              | sed 's|^git+https://codeberg.org/|git+ssh://git@codeberg.org/|' \
              | sed 's/%2F/\//g; s/%23/#/g; s/%3F/?/g; s/%3D/=/g; s/%26/\&/g' \
              | sort > "$TMPDIR/sidecar_keys"
          else
            touch "$TMPDIR/sidecar_keys"
          fi

          comm -23 "$TMPDIR/lock_sources" "$TMPDIR/sidecar_keys" > "$TMPDIR/missing"
          if [[ -s "$TMPDIR/missing" ]]; then
            echo "ERROR: Lock file has sources without sidecar entries:" >&2
            while IFS= read -r line; do
              echo "  $line" >&2
            done < "$TMPDIR/missing"
            echo "" >&2
            echo "Fix: run 'nix run .#cache-pin-source-pins-${escapeShellArg name}'" >&2
            exit 1
          fi

          comm -13 "$TMPDIR/lock_sources" "$TMPDIR/sidecar_keys" > "$TMPDIR/stale"
          if [[ -s "$TMPDIR/stale" ]]; then
            echo "NOTE: Sidecar has entries not in lock file (possibly stale):" >&2
            while IFS= read -r line; do
              echo "  $line" >&2
            done < "$TMPDIR/stale"
          fi

          touch "$out"
        '';

      sourcePinCoverageChecks = mapAttrs mkSourcePinCoverage cfg.source-pins;

      sourcePinBinPath = lib.makeBinPath (builtins.attrValues sourcePinUpdateScripts);

      sourcePinAllUpdate = pkgs.writeShellScriptBin "cache-pin-source-pins" ''
        set -euo pipefail
        export PATH="${sourcePinBinPath}:$PATH"
        results=()
        for n in ${concatStringsSep " " (builtins.attrNames cfg.source-pins)}; do
          echo ""
          if "cache-pin-source-pins-$n"; then
            results+=("$n: OK")
          else
            results+=("$n: FAILED")
          fi
        done
        echo ""
        echo "=== source-pins summary ==="
        printf '%s\n' "''${results[@]}"
      '';
    in {
      apps =
        (lib.mapAttrs' (name: drv:
          lib.nameValuePair "cache-pin-${name}" {
            type = "app";
            program = "${drv}/bin/cache-pin-${name}";
          })
        pinApps)
        // (lib.mapAttrs' (name: drv:
          lib.nameValuePair "cache-pin-source-pins-${name}" {
            type = "app";
            program = "${drv}/bin/cache-pin-source-pins-${name}";
          })
        sourcePinUpdateScripts)
        // {
          cache-pin = {
            type = "app";
            program = "${allApp}/bin/cache-pin";
          };
          cache-pin-update = {
            type = "app";
            program = "${updateAllApp}/bin/cache-pin-update";
          };
          cache-pin-source-pins = {
            type = "app";
            program = "${sourcePinAllUpdate}/bin/cache-pin-source-pins";
          };
        };

      checks = sourcePinCoverageChecks;
    };
  };
}
