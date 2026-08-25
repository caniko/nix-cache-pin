{
  description = "Pin flake inputs to nixpkgs revisions where your packages have binary cache hits";

  inputs = {
    rs-harbor.url = "git+https://github.com/caniko/rs-harbor.git?ref=trunk&rev=05cc4f162b55fa904b687db1821e2463fa813e50";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    crane.url = "github:ipetkov/crane";
    plinth = {
      url = "git+https://github.com/caniko/plinth.git?ref=refs/heads/trunk";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs:
    inputs.flake-parts.lib.mkFlake {inherit inputs;} {
      # nix-cache-pin is a workstation/server CLI tool. No aarch64-linux
      # consumer exists in caniko's fleet (thething doesn't pin caches),
      # so evaluating aarch64 outputs is dead weight that doubles
      # `nix flake check` heap for nothing.
      systems = ["x86_64-linux"];

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
          overlays = [(import inputs.rs-harbor.inputs.rust-overlay)];
        };
        toolchain = inputs.rs-harbor.lib.mkToolchain { pkgs = pkgsWithRust; toolchainProfile = "nightly"; };
        craneLib = toolchain.craneLib;
        buildCache = inputs.rs-harbor.lib.mkBuildCachePolicy {
          inherit pkgs;
          sccachePackage = inputs.rs-harbor.packages.${system}.sccache;
          cacheRoot = null;
          namespaceScope = "canix-rust";
          namespaceGeneration = 5;
        };
        src = craneLib.cleanCargoSource ./.;

        commonArgs = {
          inherit src;
          pname = "nix-cache-pin";
          version = "0.1.0";
          strictDeps = true;
          nativeBuildInputs = [pkgs.pkg-config];
          buildInputs =
            [pkgs.openssl]
            ++ lib.optionals pkgs.stdenv.isDarwin [
              pkgs.libiconv
            ];
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        individualCrateArgs = commonArgs // {
          inherit cargoArtifacts;
          version = "0.1.0";
          doCheck = false;
        };

        fileSetForCrate = crate:
          lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              (craneLib.fileset.commonCargoSources ./crates/nix-cache-pin-lib)
              (craneLib.fileset.commonCargoSources crate)
            ];
          };

        cache-pin = buildCache.withRustCache { package = craneLib.buildPackage (individualCrateArgs
          // {
            pname = "cache-pin";
            cargoExtraArgs = "-p cache-pin";
            src = fileSetForCrate ./crates/cache-pin;
          }); };

        narinfo-check = buildCache.withRustCache { package = craneLib.buildPackage (individualCrateArgs
          // {
            pname = "narinfo-check";
            cargoExtraArgs = "-p narinfo-check";
            src = fileSetForCrate ./crates/narinfo-check;
          }); };

        hydra-query = buildCache.withRustCache { package = craneLib.buildPackage (individualCrateArgs
          // {
            pname = "hydra-query";
            cargoExtraArgs = "-p hydra-query";
            src = fileSetForCrate ./crates/hydra-query;
          }); };

        nix-eval-store-path = buildCache.withRustCache { package = craneLib.buildPackage (individualCrateArgs
          // {
            pname = "nix-eval-store-path";
            cargoExtraArgs = "-p nix-eval-store-path";
            src = fileSetForCrate ./crates/nix-eval-store-path;
          }); };

        # Combined package with all binaries for module.nix runtime
        all-binaries = pkgs.symlinkJoin {
          name = "nix-cache-pin";
          paths = [cache-pin narinfo-check hydra-query nix-eval-store-path];
        };
        website = inputs.plinth.lib.${system}.mkProjectSite {
          pname = "nix-cache-pin-website";
          domain = "nix-cache-pin.tartanoglu.com";
          configPath = ./website/plinth-project.toml;
        };
      in {
        formatter = pkgs.alejandra;

        packages = {
          inherit cache-pin narinfo-check hydra-query nix-eval-store-path all-binaries website;
          default = all-binaries;
          site = website;
        };

        apps.deploy-pages = inputs.plinth.lib.${system}.mkDeployPagesApp {
          domain = "nix-cache-pin.tartanoglu.com";
        };

        checks =
          {
            inherit cache-pin narinfo-check hydra-query nix-eval-store-path;

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
                partitions = 1;
                partitionType = "count";
                cargoNextestPartitionsExtraArgs = "--no-tests=pass";
              });
          }
          // (import ./tests/module-eval.nix {inherit lib pkgs;})
          // (import ./tests/flake-module.nix {
            inherit lib pkgs;
            flake-parts-lib = inputs.flake-parts.lib;
          });

        devShells.default = craneLib.devShell {
          packages = [inputs.rs-harbor.packages.${system}.harbor-ci] ++ (with pkgs; [
            nix
            git
            gh
            curl
            pkg-config
            openssl
          ]);
        };
      };
    };
}
