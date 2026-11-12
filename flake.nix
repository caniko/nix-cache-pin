{
  description = "Pin flake inputs to nixpkgs revisions where your packages have binary cache hits";

  inputs = {
    harbor-rs.url = "git+https://github.com/caniko/harbor-rs.git?ref=trunk&rev=7f03d58b0e6e748b4817417ff21958e074ec6faf";
    # harbor-rs keeps its dev-only outputs (formatter, checks, devShells) in a
    # partition with its own lock, so consumers own this input.
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    crane.url = "github:ipetkov/crane";
  };

  outputs = inputs:
    inputs.flake-parts.lib.mkFlake {inherit inputs;} {
      # nix-cache-pin is a workstation/server CLI tool. No aarch64-linux
      # consumer exists in caniko's fleet (thething doesn't pin caches),
      # so evaluating aarch64 outputs is dead weight that doubles
      # `nix flake check` heap for nothing. aarch64-darwin is listed only so
      # the release job can build the bundle's Mach-O archive on a macOS
      # runner; no check or dev shell is expected to run there.
      systems = ["x86_64-linux" "aarch64-darwin"];

      flake.flakeModules.default = import ./nix/module.nix {cachePinSelf = inputs.self;};
      flake.presets = import ./nix/presets.nix;

      perSystem = {
        pkgs,
        lib,
        system,
        ...
      }: let
        pkgsWithRust = import inputs.nixpkgs {
          inherit system;
          overlays = [(import inputs.harbor-rs.inputs.rust-overlay)];
        };
        toolchain = inputs.harbor-rs.lib.mkToolchain {
          pkgs = pkgsWithRust;
          toolchainProfile = "nightly";
        };
        inherit (toolchain) craneLib;
        treefmtEval = inputs.treefmt-nix.lib.evalModule pkgs (import ./nix/treefmt.nix {
          rustfmtPackage = toolchain.rustToolchain;
        });
        # harbor-rs publishes its sccache and CI tooling for Linux only. macOS
        # exists here solely to build the release bundle, so the compiler cache
        # degrades to a no-op there instead of breaking evaluation.
        harborPackages = inputs.harbor-rs.packages.${system} or {};
        buildCache =
          if harborPackages ? sccache
          then
            inputs.harbor-rs.lib.mkBuildCachePolicy {
              inherit pkgs;
              sccachePackage = harborPackages.sccache;
              # Redis wins when mounted; hosted builders fall back to this sandbox-local cache.
              cacheRoot = "/build/cache";
              namespaceScope = "canix-rust";
              namespaceGeneration = 5;
            }
          else {withRustCache = attrs: attrs.package;};
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            (craneLib.fileset.commonCargoSources ./.)
            ./crates/nix-cache-pin-lib/tests/fixtures
          ];
        };

        commonArgs = {
          inherit src;
          pname = "nix-cache-pin";
          version = "0.3.3";
          strictDeps = true;
          buildInputs = lib.optionals pkgs.stdenv.hostPlatform.isDarwin [
            pkgs.libiconv
          ];
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        individualCrateArgs =
          commonArgs
          // {
            inherit cargoArtifacts;
            version = "0.3.3";
            doCheck = false;
          };

        fileSetForCrate = crate:
          lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              (craneLib.fileset.commonCargoSources ./crates/nix-cache-pin-lib)
              ./crates/nix-cache-pin-lib/tests/fixtures
              (craneLib.fileset.commonCargoSources crate)
            ];
          };

        cache-pin = buildCache.withRustCache {
          package = craneLib.buildPackage (individualCrateArgs
            // {
              pname = "cache-pin";
              cargoExtraArgs = "-p cache-pin";
              src = fileSetForCrate ./crates/cache-pin;
            });
        };

        narinfo-check = buildCache.withRustCache {
          package = craneLib.buildPackage (individualCrateArgs
            // {
              pname = "narinfo-check";
              cargoExtraArgs = "-p narinfo-check";
              src = fileSetForCrate ./crates/narinfo-check;
            });
        };

        hydra-query = buildCache.withRustCache {
          package = craneLib.buildPackage (individualCrateArgs
            // {
              pname = "hydra-query";
              cargoExtraArgs = "-p hydra-query";
              src = fileSetForCrate ./crates/hydra-query;
            });
        };

        nix-eval-store-path = buildCache.withRustCache {
          package = craneLib.buildPackage (individualCrateArgs
            // {
              pname = "nix-eval-store-path";
              cargoExtraArgs = "-p nix-eval-store-path";
              src = fileSetForCrate ./crates/nix-eval-store-path;
            });
        };

        # Combined package with all binaries for module.nix runtime
        all-binaries = pkgs.symlinkJoin {
          name = "nix-cache-pin";
          paths = [cache-pin narinfo-check hydra-query nix-eval-store-path];
        };
        website = pkgs.runCommand "nix-cache-pin-website" {} ''
          cp -r --no-preserve=mode ${./website/public} $out
          printf '%s\n' nix-cache-pin.tartanoglu.com > $out/.domains
        '';

        # cargo-binstall archives plus the [package.metadata.binstall] the
        # crates carry. Static musl archives cover glibc and musl Linux hosts;
        # the workspace cannot build for Windows because the library uses
        # std::os::unix and the `nix` crate.
        release = inputs.harbor-rs.lib.mkBinstallRelease {
          # mkToolchain (used for every cross target) needs rust-overlay.
          pkgs = pkgsWithRust;
          inherit system;
          inherit (commonArgs) pname version;
          # One release binary per target; the workspace test suite needs git,
          # nix and network fixtures that an archive build does not provide.
          commonArgs = commonArgs // {doCheck = false;};
          crates = {
            cache-pin.cargoToml = ./crates/cache-pin/Cargo.toml;
            hydra-query.cargoToml = ./crates/hydra-query/Cargo.toml;
            narinfo-check.cargoToml = ./crates/narinfo-check/Cargo.toml;
            nix-eval-store-path.cargoToml = ./crates/nix-eval-store-path/Cargo.toml;
          };
          targets = {
            x86_64-linux = ["x86_64-linux-musl" "aarch64-linux-musl"];
            aarch64-darwin = ["native"];
          };
        };
      in {
        formatter = treefmtEval.config.build.wrapper;

        packages =
          {
            inherit cache-pin narinfo-check hydra-query nix-eval-store-path all-binaries website;
            default = all-binaries;
            site = website;
          }
          // lib.optionalAttrs (release.releaseBundle != null) {
            release-bundle = release.releaseBundle;
          };

        checks =
          {
            inherit cache-pin narinfo-check hydra-query nix-eval-store-path;

            formatting = treefmtEval.config.build.check inputs.self;

            workspace-clippy = craneLib.cargoClippy (commonArgs
              // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets -- --deny warnings";
              });

            workspace-fmt = craneLib.cargoFmt {
              inherit src;
            };

            workspace-test = craneLib.cargoNextest (commonArgs
              // {
                inherit cargoArtifacts;
                # Mutation tests resolve the repository-wide lock with Git.
                nativeCheckInputs = [pkgs.git pkgs.nix];
                # The platform verifier needs real roots even for HTTP mock clients.
                # Sandboxes cannot read the host's system trust store.
                SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
                partitions = 1;
                partitionType = "count";
                cargoNextestPartitionsExtraArgs = "--no-tests=pass";
              });
          }
          // {
            # The real binary verifies a recorded PyTorch ROCm wheel-pin sidecar offline and
            # rejects a drifted copy (declared constraint no longer satisfied).
            wheel-pins-verify-fixture = pkgs.runCommand "cache-pin-wheel-pins-verify-fixture" {nativeBuildInputs = [pkgs.jq];} ''
              fixtures=${./tests/fixtures/wheel-pins}
              ${cache-pin}/bin/cache-pin-wheel-pins --config $fixtures/rocm-config.json --verify-sidecar $fixtures/rocm.json
              jq '.packages.torch = ">=2.15"' $fixtures/rocm-config.json > drifted.json
              if ${cache-pin}/bin/cache-pin-wheel-pins --config drifted.json --verify-sidecar $fixtures/rocm.json; then
                echo "drifted constraint was accepted" >&2
                exit 1
              fi
              touch $out
            '';
          }
          // (import ./tests/module-eval.nix {inherit lib pkgs;})
          // (import ./tests/flake-module.nix {
            inherit lib pkgs;
            flake-parts-lib = inputs.flake-parts.lib;
          })
          // release.checks;

        devShells.default = craneLib.devShell {
          packages =
            lib.optionals (harborPackages ? harbor-ci) [harborPackages.harbor-ci]
            ++ [treefmtEval.config.build.wrapper]
            ++ (with pkgs; [
              nix
              git
              gh
              curl
              cargo-nextest
              cargo-audit
              cargo-deny
            ]);
        };
      };
    };
}
