{
  description = "frances - an agentic coding tool";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        frances = rustPlatform.buildRustPackage {
          pname = "frances";
          version = "0.1.0";
          src = self;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          cargoBuildFlags = [ "-p" "frances" ];
          cargoTestFlags = [ "-p" "frances" ];

          meta = with pkgs.lib; {
            description = "frances - an agentic coding tool";
            mainProgram = "frances";
            license = licenses.mit;
            platforms = platforms.all;
          };
        };
      in {
        packages = {
          default = frances;
          inherit frances;
        };

        devShells.default = pkgs.mkShell {
          packages = [
            rustToolchain
            pkgs.rust-analyzer
            pkgs.jq
            pkgs.python3
            pkgs.cargo-nextest
            pkgs.cargo-machete
          ];
        };
      });
}
