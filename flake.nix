{
  description = "uplink — self-hosted personal communication hub";

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
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "clippy" "rustfmt" ];
        };

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # ── web (SPA) ──────────────────────────────────────────────────────
        # Build the Preact SPA via npm (buildNpmPackage is well-tested in nixpkgs).
        # pnpm is used in development; npm is used for the Nix build.
        # Update npmDepsHash with: nix build .#uplinkWeb 2>&1 | grep "got:"
        uplinkWeb = pkgs.buildNpmPackage {
          pname = "uplink-web";
          version = "0.1.0";
          src = ./web;
          npmDepsHash = "sha256-20dwDHil36WVB72XC0L5XXjNSQ/owY4M6EiYQBSUWGQ=";
          buildPhase = ''
            npm run build -- --outDir $out
          '';
          installPhase = "true";
          dontNpmInstall = true;
        };

        # ── Rust ───────────────────────────────────────────────────────────
        commonArgs = {
          src = craneLib.cleanCargoSource ./.;
          strictDeps = true;
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [
            pkgs.openssl
            pkgs.sqlite
          ] ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            pkgs.libiconv
          ];
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        # Binary with embedded SPA (rust-embed reads crates/uplink-server/static/).
        uplinkBin = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          preBuild = ''
            mkdir -p crates/uplink-server/static
            cp -r ${uplinkWeb}/. crates/uplink-server/static/
          '';
          cargoExtraArgs = "--bin uplink";
        });

      in {
        # ── packages ───────────────────────────────────────────────────────
        packages = {
          default = uplinkBin;
          uplink = uplinkBin;
        };

        # ── dev shell ──────────────────────────────────────────────────────
        devShells.default = craneLib.devShell {
          checks = self.checks.${system};
          packages = [
            pkgs.nodejs
            pkgs.pnpm_10
            pkgs.openssl
            pkgs.sqlite
            pkgs.pkg-config
            pkgs.rust-analyzer
          ];
          shellHook = ''
            echo "uplink dev — rustc $(rustc --version), node $(node --version)"
          '';
        };

        # ── checks (nix flake check) ───────────────────────────────────────
        checks = {
          backend-test = craneLib.cargoTest (commonArgs // {
            inherit cargoArtifacts;
          });

          backend-lint = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--workspace -- -D warnings";
          });

          # pnpm build is used in the dev workflow; npm build is used in Nix.
          # This check verifies the npm-based build produces a valid output.
          frontend-build = pkgs.buildNpmPackage {
            pname = "uplink-frontend-check";
            version = "0.1.0";
            src = ./web;
            npmDepsHash = "sha256-20dwDHil36WVB72XC0L5XXjNSQ/owY4M6EiYQBSUWGQ=";
            buildPhase = ''
              mkdir -p $out
              npm run build -- --outDir $out
            '';
            installPhase = "true";
            dontNpmInstall = true;
          };
        };
      });
}
