{lib, ...}: let
  inherit
    (lib)
    mkOption
    types
    mapAttrs
    mapAttrsToList
    concatStringsSep
    ;

  sourcePinSubmodule = types.submodule (
    {name, ...}: {
      options = {
        type = mkOption {
          type = types.enum ["cargo-git"];
          description = "Lock file format for source extraction.";
        };

        lockFile = mkOption {
          type = types.path;
          description = "Lock file to extract sources from.";
          example = ./cli/Cargo.lock;
        };

        outputFile = mkOption {
          type = types.str;
          description = ''
            Relative path from flake root to the generated Nix sidecar.
            The sidecar is a checked-in Nix file mapping source strings
            to narHashes, imported by the consuming crane build.
          '';
          example = "cli/cargo-git-hashes.nix";
        };
      };
    }
  );
in {
  options.cache-pin.source-pins = mkOption {
    type = types.attrsOf sourcePinSubmodule;
    default = {};
    description = "Set of source pins to manage (e.g. Cargo.lock git deps).";
    example = {
      "cargo-git-deps" = {
        type = "cargo-git";
        lockFile = ./cli/Cargo.lock;
        outputFile = "cli/cargo-git-hashes.nix";
      };
    };
  };
}
