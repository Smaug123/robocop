{
  description = "GitHub Code Review Bot development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "clippy" ];
        };
        
        robocop-server = pkgs.rustPlatform.buildRustPackage {
          pname = "robocop-server";
          version = "0.1.0";

          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          nativeBuildInputs = with pkgs; [
            pkg-config
          ];

          buildInputs = with pkgs; [
            openssl
            libiconv
          ];

          # Build only the server binary
          buildAndTestSubdir = "robocop-server";

          meta = with pkgs.lib; {
            description = "Robocop GitHub Code Review Server";
            homepage = "https://github.com/Smaug123/robocop";
            license = licenses.mit;
            maintainers = [ ];
          };
        };

        robocop-cli = pkgs.rustPlatform.buildRustPackage {
          pname = "robocop";
          version = "0.1.0";

          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          nativeBuildInputs = with pkgs; [
            pkg-config
          ];

          buildInputs = with pkgs; [
            openssl
            libiconv
          ];

          # Build only the CLI binary
          buildAndTestSubdir = "robocop-cli";

          meta = with pkgs.lib; {
            description = "Robocop Code Review CLI";
            homepage = "https://github.com/Smaug123/robocop";
            license = licenses.mit;
            maintainers = [ ];
          };
        };
      in
      {
        packages = {
          default = robocop-server;
          robocop-cli = robocop-cli;
          robocop-server = robocop-server;
          # Alias for backwards compatibility
          github-bot = robocop-server;
        };
        
        devShells.default = pkgs.mkShell {
          buildInputs = [
            rustToolchain
            pkgs.cargo
            pkgs.pkg-config
            pkgs.openssl
            pkgs.libiconv
            pkgs.uv
          ];

          RUST_BACKTRACE = "1";
        };
      });
}
