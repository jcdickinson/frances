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

        tauriNativeBuildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux (with pkgs; [
          pkg-config
          wrapGAppsHook3
        ]);

        tauriBuildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux (with pkgs; [
          dbus
          gtk3
          libsoup_3
          webkitgtk_4_1
        ]);

        frances = rustPlatform.buildRustPackage {
          pname = "frances";
          version = "0.1.0";
          src = self;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          cargoBuildFlags = [ "-p" "frances" ];
          cargoTestFlags = [ "-p" "frances" ];

          nativeBuildInputs = tauriNativeBuildInputs;
          buildInputs = tauriBuildInputs;

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
          packages = (with pkgs; [
            rustToolchain
            rust-analyzer
            jq
            python3
            cargo-nextest
            cargo-machete
            just
            deno
          ]) ++ tauriNativeBuildInputs ++ tauriBuildInputs;
        };
      });
}
