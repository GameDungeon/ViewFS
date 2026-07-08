{
  lib,
  rustPlatform,
}: let
  cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
in
  rustPlatform.buildRustPackage (finalAttrs: {
    pname = cargoToml.package.name;
    inherit (cargoToml.package) version;

    cargoLock = {
      lockFile = ./Cargo.lock;
      outputHashes = {
        "fuser-0.17.0" = "sha256-X0gmp37Lw17OsR0G1NYaVtEs+zSbUPt+iLPJgDoO7Zw=";
      };
    };

    src = lib.sourceFilesBySuffices ./. [
      ".rs"
      ".toml"
      ".lock"
    ];
  })
