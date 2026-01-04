{
  description = "GitHub Code Review Bot development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, crane, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
          config.allowUnfree = true;
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "clippy" ];
        };

        craneLib = (crane.mkLib pkgs).overrideToolchain (_: rustToolchain);

        # Common source filtering for all builds
        # Include standard Cargo sources plus prompt.txt (used by include_str!)
        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter = path: type:
            (craneLib.filterCargoSources path type)
            || (builtins.baseNameOf path == "prompt.txt");
        };

        # Common build inputs
        commonBuildInputs = with pkgs; [
          openssl
          libiconv
        ];

        commonNativeBuildInputs = with pkgs; [
          pkg-config
        ];

        # Common arguments shared between buildDepsOnly and buildPackage
        commonArgs = {
          inherit src;
          strictDeps = true;
          buildInputs = commonBuildInputs;
          nativeBuildInputs = commonNativeBuildInputs;
          # Workspace doesn't have a package version; set explicitly
          version = "0.1.0";
        };

        # Build *only* the dependencies - this derivation gets cached
        cargoArtifacts = craneLib.buildDepsOnly (commonArgs // {
          pname = "robocop-deps";
        });

        robocop-server = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          pname = "robocop-server";

          # Pass git revision to the build
          ROBOCOP_GIT_HASH = if (self ? rev) && (self.rev != null) then self.rev else "dirty";

          # Build only the server binary
          cargoExtraArgs = "--locked -p robocop-server";

          meta = with pkgs.lib; {
            description = "Robocop GitHub Code Review Server";
            homepage = "https://github.com/Smaug123/robocop";
            license = licenses.mit;
            maintainers = [ ];
          };
        });

        robocop-cli = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          pname = "robocop-cli";

          # Build only the CLI binary
          cargoExtraArgs = "--locked -p robocop-cli";

          meta = with pkgs.lib; {
            description = "Robocop Code Review CLI";
            homepage = "https://github.com/Smaug123/robocop";
            license = licenses.mit;
            maintainers = [ ];
          };
        });
      in
      {
        packages = {
          default = robocop-server;
          robocop-cli = robocop-cli;
          robocop-server = robocop-server;
          # Alias for backwards compatibility
          github-bot = robocop-server;
        };

        devShells.default = craneLib.devShell {
          packages = [
            pkgs.pkg-config
            pkgs.openssl
            pkgs.libiconv
            pkgs.claude-code
            pkgs.codex
          ];

          RUST_BACKTRACE = "1";
        };
      });
}
