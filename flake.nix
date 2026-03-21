{
  description = "Pin flake inputs to nixpkgs revisions where your packages have binary cache hits";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    crane.url = "github:ipetkov/crane";
  };

  outputs = inputs:
    inputs.flake-parts.lib.mkFlake {inherit inputs;} {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      flake.flakeModules.default = import ./nix/module.nix {cachePinSelf = inputs.self;};
      flake.presets = import ./nix/presets.nix;

      perSystem = {
        pkgs,
        lib,
        system,
        ...
      }: let
        craneLib = inputs.crane.mkLib pkgs;
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

        cache-pin = craneLib.buildPackage (individualCrateArgs
          // {
            pname = "cache-pin";
            cargoExtraArgs = "-p cache-pin";
            src = fileSetForCrate ./crates/cache-pin;
          });

        narinfo-check = craneLib.buildPackage (individualCrateArgs
          // {
            pname = "narinfo-check";
            cargoExtraArgs = "-p narinfo-check";
            src = fileSetForCrate ./crates/narinfo-check;
          });

        hydra-query = craneLib.buildPackage (individualCrateArgs
          // {
            pname = "hydra-query";
            cargoExtraArgs = "-p hydra-query";
            src = fileSetForCrate ./crates/hydra-query;
          });

        nix-eval-store-path = craneLib.buildPackage (individualCrateArgs
          // {
            pname = "nix-eval-store-path";
            cargoExtraArgs = "-p nix-eval-store-path";
            src = fileSetForCrate ./crates/nix-eval-store-path;
          });

        # Combined package with all binaries for module.nix runtime
        all-binaries = pkgs.symlinkJoin {
          name = "nix-cache-pin";
          paths = [cache-pin narinfo-check hydra-query nix-eval-store-path];
        };
      in {
        formatter = pkgs.alejandra;

        packages = {
          inherit cache-pin narinfo-check hydra-query nix-eval-store-path all-binaries;
          default = all-binaries;
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
          packages = with pkgs; [
            nix
            git
            gh
            curl
            pkg-config
            openssl
          ];
        };
      };
    };
}
