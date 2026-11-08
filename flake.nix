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
        # When web/pnpm-lock.yaml changes, get the new hash with
        #   nix build .#uplinkWebDeps --rebuild 2>&1 | grep got:
        # (--rebuild: the output for the old hash satisfies a plain build).
        uplinkWebDeps = pkgs.fetchPnpmDeps {
          pname = "uplink-web-deps";
          src = ./web;
          fetcherVersion = 3;
          pnpm = pkgs.pnpm_11;
          hash = "sha256-9mKBJXvYv+pYJyB4/q3MfQYPyJwSH7snRkGcYTZzyxE=";
        };

        # A derivation running `script` in web/ with its dependencies.
        webDerivation = pname: script: pkgs.stdenv.mkDerivation {
          inherit pname;
          version = "0.1.0";
          src = ./web;
          nativeBuildInputs = [ pkgs.nodejs pkgs.pnpm_11 pkgs.pnpmConfigHook ];
          pnpmDeps = uplinkWebDeps;
          buildPhase = script;
          installPhase = "true";
          dontInstall = true;
        };

        uplinkWeb = webDerivation "uplink-web" ''
          pnpm exec tsc
          pnpm exec vite build --outDir $out
        '';

        # ── Rust ───────────────────────────────────────────────────────────
        # Cargo sources plus the SQL migrations that uplink-storage embeds
        # at compile time, and the example configuration.
        cargoSrc = pkgs.lib.cleanSourceWith {
          src = ./.;
          name = "source";
          filter = path: type:
            pkgs.lib.hasSuffix ".sql" path
            # A test checks that the example configuration parses.
            || pkgs.lib.hasSuffix "/examples/uplink.yaml" path
            || craneLib.filterCargoSources path type;
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
        # Conduit insists on reading /etc/resolv.conf, which build sandboxes
        # lack; with federation off, it never resolves anything.
        conduit = pkgs.writeShellScriptBin "conduit" ''
          export LD_PRELOAD=${pkgs.libredirect}/lib/libredirect.so
          export NIX_REDIRECTS=/etc/resolv.conf=${pkgs.writeText "resolv.conf" "nameserver 127.0.0.1\n"}
          exec ${pkgs.matrix-conduit}/bin/conduit "$@"
        '';
        # Provider-facing Rust tests run against these (crates/*/tests).
        testEnv = {
          UPLINK_EMULATE = "${emulate}/bin/emulate";
          # A Matrix homeserver, for crates/uplink-matrix/tests.
          UPLINK_CONDUIT = "${conduit}/bin/conduit";
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
            pkgs.pnpm_11
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

          # Type-checks and bundles the SPA.
          frontend-build = uplinkWeb;

          frontend-lint = webDerivation "uplink-web-lint" ''
            pnpm lint
            mkdir $out
          '';

          frontend-test = webDerivation "uplink-web-test" ''
            pnpm test
            mkdir $out
          '';
        } // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          # NixOS VM tests (SPEC §15.10).
          integration-gmail = import ./nix/tests/gmail.nix {
            inherit pkgs emulate;
            uplinkModule = self.nixosModules.default;
          };
          integration-m365 = import ./nix/tests/m365.nix {
            inherit pkgs emulate;
            uplinkModule = self.nixosModules.default;
          };
          e2e = import ./nix/tests/e2e.nix {
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
