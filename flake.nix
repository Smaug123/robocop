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
    let
      mkPackages = pkgs:
        let
          pkgs' = pkgs.extend (import rust-overlay);

          rustToolchain = pkgs'.rust-bin.stable.latest.default.override {
            extensions = [ "rust-src" "clippy" ];
          };

          craneLib = (crane.mkLib pkgs').overrideToolchain (_: rustToolchain);

          src = pkgs'.lib.cleanSourceWith {
            src = ./.;
            filter = path: type:
              let
                baseName = builtins.baseNameOf path;
                isIncludeStrFile = pkgs'.lib.hasSuffix ".txt" baseName
                                || pkgs'.lib.hasSuffix ".html" baseName;
              in
              (craneLib.filterCargoSources path type) || isIncludeStrFile;
          };

          commonBuildInputs = with pkgs'; [
            openssl
            libiconv
          ];

          commonNativeBuildInputs = with pkgs'; [
            pkg-config
          ];

          commonArgs = {
            inherit src;
            strictDeps = true;
            buildInputs = commonBuildInputs;
            nativeBuildInputs = commonNativeBuildInputs;
            version = "0.1.0";
          };

          cargoArtifacts = craneLib.buildDepsOnly (commonArgs // {
            pname = "robocop-deps";
            cargoExtraArgs = "--locked --workspace";
          });

          robocop-server = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            pname = "robocop-server";

            ROBOCOP_GIT_HASH = if (self ? rev) && (self.rev != null) then self.rev else "dirty";

            cargoExtraArgs = "--locked -p robocop-server";

            meta = with pkgs'.lib; {
              description = "Robocop GitHub Code Review Server";
              homepage = "https://github.com/Smaug123/robocop";
              license = licenses.mit;
              maintainers = [ ];
            };
          });

          robocop-cli = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            pname = "robocop-cli";

            cargoExtraArgs = "--locked -p robocop-cli";

            meta = with pkgs'.lib; {
              description = "Robocop Code Review CLI";
              homepage = "https://github.com/Smaug123/robocop";
              license = licenses.mit;
              maintainers = [ ];
            };
          });
        in
        {
          default = robocop-server;
          robocop-cli = robocop-cli;
          robocop-server = robocop-server;
          github-bot = robocop-server;
        };
    in
    {
      lib.mkPackages = mkPackages;
    }
    // flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          config.allowUnfree = true;
        };
        pkgs' = pkgs.extend (import rust-overlay);
        rustToolchain = pkgs'.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "clippy" ];
        };
        craneLib = (crane.mkLib pkgs').overrideToolchain (_: rustToolchain);
      in
      {
        packages = mkPackages pkgs;

        devShells.default = craneLib.devShell {
          packages = with pkgs'; [
            pkg-config
            openssl
            libiconv
            claude-code
            codex
          ];

          RUST_BACKTRACE = "1";
        };
      });
}
