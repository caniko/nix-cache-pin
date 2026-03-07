{
  description = "Pin flake inputs to nixpkgs revisions where your packages have binary cache hits";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
  };

  outputs = inputs:
    inputs.flake-parts.lib.mkFlake {inherit inputs;} {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      flake.flakeModules.default = ./nix/module.nix;
      flake.presets = import ./nix/presets.nix;

      perSystem = {
        pkgs,
        lib,
        ...
      }: let
        # Bundle test + script together so `source ../scripts/cache-pin.nu` resolves
        e2eTestDir = pkgs.runCommand "cache-pin-e2e-dir" {} ''
          mkdir -p $out/tests $out/scripts
          cp ${./tests/e2e.nu} $out/tests/e2e.nu
          cp ${./scripts/cache-pin.nu} $out/scripts/cache-pin.nu
        '';
        e2eTest = pkgs.writeShellScriptBin "cache-pin-e2e-test" ''
          export PATH="${lib.makeBinPath (with pkgs; [nushell curl])}:$PATH"
          exec ${pkgs.nushell}/bin/nu ${e2eTestDir}/tests/e2e.nu
        '';
      in {
        formatter = pkgs.alejandra;

        apps.e2e-test = {
          type = "app";
          program = "${e2eTest}/bin/cache-pin-e2e-test";
        };

        checks =
          (import ./tests/module-eval.nix {inherit lib pkgs;})
          // (import ./tests/flake-module.nix {
            inherit lib pkgs;
            flake-parts-lib = inputs.flake-parts.lib;
          })
          // (import ./tests/script-syntax.nix {inherit pkgs;});
      };
    };
}
