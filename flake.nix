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
        # pnpm_9 (9.x) produces a JSON-based v10 store that fetchPnpmDeps
        # handles correctly; pnpm_10/11 produces a SQLite v11 store that
        # is not fully supported by the fixup phase yet.
        # Update pnpmDepsHash with: nix build .#uplinkWebDeps 2>&1 | grep "got:"
        uplinkWebDeps = pkgs.fetchPnpmDeps {
          pname = "uplink-web-deps";
          src = ./web;
          fetcherVersion = 3;
          pnpm = pkgs.pnpm_9;
          hash = "sha256-HN/XY+FjaI25/xW1oJ4sjOfYioEIiIPayDH6s+FWYHc=";
        };

        uplinkWeb = pkgs.stdenv.mkDerivation {
          pname = "uplink-web";
          version = "0.1.0";
          src = ./web;
          nativeBuildInputs = [ pkgs.nodejs pkgs.pnpm_9 pkgs.pnpmConfigHook ];
          pnpmDeps = uplinkWebDeps;
          buildPhase = ''
            pnpm exec tsc
            pnpm exec vite build --outDir $out
          '';
          installPhase = "true";
          dontInstall = true;
        };

        # ── Rust ───────────────────────────────────────────────────────────
        # Cargo sources plus the SQL migrations that uplink-storage embeds
        # at compile time.
        cargoSrc = pkgs.lib.cleanSourceWith {
          src = ./.;
          name = "source";
          filter = path: type:
            pkgs.lib.hasSuffix ".sql" path || craneLib.filterCargoSources path type;
        };

        commonArgs = {
          src = cargoSrc;
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
          meta.mainProgram = "uplink";
        });

        # Provider fakes for integration tests.
        emulate = pkgs.callPackage ./nix/emulate { };
        # Provider-facing Rust tests run against these (crates/*/tests).
        testEnv = {
          UPLINK_EMULATE = "${emulate}/bin/emulate";
        };

      in {
        # ── packages ───────────────────────────────────────────────────────
        packages = {
          inherit emulate;
          default = uplinkBin;
          uplink = uplinkBin;
          uplinkWeb = uplinkWeb;
          uplinkWebDeps = uplinkWebDeps;
        };

        # ── dev shell ──────────────────────────────────────────────────────
        devShells.default = craneLib.devShell (testEnv // {
          checks = self.checks.${system};
          packages = [
            pkgs.nodejs
            pkgs.pnpm_9
            pkgs.openssl
            pkgs.sqlite
            pkgs.pkg-config
            pkgs.rust-analyzer
          ];
          shellHook = ''
            echo "uplink dev — rustc $(rustc --version), node $(node --version)"
          '';
        });

        # ── checks (nix flake check) ───────────────────────────────────────
        checks = {
          backend-test = craneLib.cargoTest (commonArgs // testEnv // {
            inherit cargoArtifacts;
            # The sandbox has no system trust store, without which
            # reqwest's platform verifier refuses to build a client.
            nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ pkgs.cacert ];
          });

          backend-lint = craneLib.cargoClippy (commonArgs // {
            inherit cargoArtifacts;
            cargoClippyExtraArgs = "--workspace -- -D warnings";
          });

          # web/src/generated/protocol.ts is generated from uplink-protocol and checked in.
          protocol-ts = craneLib.mkCargoDerivation (commonArgs // {
            inherit cargoArtifacts;
            pnameSuffix = "-protocol-ts";
            buildPhaseCargoCommand = ''
              cargoWithProfile run -q -p uplink-protocol --example typescript > protocol.ts
              diff -u ${./web/src/generated/protocol.ts} protocol.ts
            '';
            installPhaseCommand = "mkdir -p $out";
          });

          backend-fmt = craneLib.cargoFmt {
            src = cargoSrc;
          };

          # Verify the pnpm-based SPA build produces valid output.
          frontend-build = pkgs.stdenv.mkDerivation {
            pname = "uplink-frontend-check";
            version = "0.1.0";
            src = ./web;
            nativeBuildInputs = [ pkgs.nodejs pkgs.pnpm_9 pkgs.pnpmConfigHook ];
            pnpmDeps = uplinkWebDeps;
            buildPhase = ''
              pnpm exec tsc
              pnpm exec vite build --outDir $out
            '';
            installPhase = "true";
            dontInstall = true;
          };
        } // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          # NixOS VM tests (SPEC §15.10).
          integration-gmail = import ./nix/tests/gmail.nix {
            inherit pkgs emulate;
            uplinkModule = self.nixosModules.default;
          };
        };
      }) // {
        nixosModules.default = import ./nix/module.nix {
          defaultPackage = pkgs: self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };
      };
}
