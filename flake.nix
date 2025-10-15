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
        
        github-bot = pkgs.rustPlatform.buildRustPackage {
          pname = "github-bot";
          version = "0.1.0";
          
          src = ./github-bot;
          
          cargoLock = {
            lockFile = ./github-bot/Cargo.lock;
          };
          
          nativeBuildInputs = with pkgs; [
            pkg-config
          ];
          
          buildInputs = with pkgs; [
            openssl
            libiconv
          ];
          
          meta = with pkgs.lib; {
            description = "GitHub Code Review Bot";
            homepage = "https://github.com/your-username/github-bot";
            license = licenses.mit;
            maintainers = [ ];
          };
        };
      in
      {
        packages = {
          default = github-bot;
          github-bot = github-bot;
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
