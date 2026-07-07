{
  description = "A fuse filesystem for views.";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    rust-overlay,
  }: let
    system = "x86_64-linux";
    pkgs = import nixpkgs {
      inherit system;
      overlays = [(import rust-overlay)];
    };
    rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
  in {
    packages.x86_64-linux = {
      default = self.packages.x86_64-linux.view-fs;
      view-fs = pkgs.callPackage ./default.nix {};
    };

    devShells.x86_64-linux.default = pkgs.mkShell {
      buildInputs = [
        pkgs.llvmPackages.bintools
        pkgs.cargo-binutils
        (rustToolchain.override {
          extensions = ["rust-analyzer" "rust-src" "clippy"];
        })
      ];
    };
  };
}
