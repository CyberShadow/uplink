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
          hash = "sha256-Fv3vPJYzhlYK5M9EYiz5LG/OJ8dJQu5PwM2/fEiOdjk=";
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
            # `uplink init` writes the example configuration, which a test
            # checks parses.
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

        # ── user service ───────────────────────────────────────────────────
        # The unit, with ExecStart already pointing into the store: `nix build`
        # is what updates it. The out-link is the build's GC root, and the unit
        # is linked through it, so the link never changes:
        #
        #   nix build -o ~/.local/state/uplink-unit .#service
        #   ln -s ~/.local/state/uplink-unit/uplink.service ~/.local/share/systemd/user/
        #   ln -s ../uplink.service ~/.local/share/systemd/user/default.target.wants/
        #   systemctl --user daemon-reload && systemctl --user start uplink
        #
        # (`systemctl enable` would link the store file itself, which the next
        # build leaves behind.)
        #
        # After a change, the first line again, then `systemctl --user
        # daemon-reload` and a restart. The file inside is named as the unit
        # because systemd names a unit after the file a link finally reaches: a
        # bare store file (`<hash>-uplink.service`) makes the link an alias of a
        # unit nobody can enable.
        #
        # The configuration is the user's, outside the checkout, so that
        # instances run from the checkout keep to their own; its `data_dir`
        # is the state directory below (an absolute path: relative ones are
        # relative to the configuration's directory).
        #
        # The out-link also carries bin/uplink, the CLI on the service's
        # configuration (`uplink oauth init --account …`), which the dotfiles
        # put on the PATH the same way.
        service = pkgs.symlinkJoin { name = "uplink-service"; paths = [ serviceUnit serviceCli ]; };

        serviceCli = pkgs.writeShellScriptBin "uplink" ''
          exec ${pkgs.lib.getExe uplinkBin} --config "''${XDG_CONFIG_HOME:-$HOME/.config}/uplink/uplink.yaml" "$@"
        '';

        serviceUnit = pkgs.writeTextDir "uplink.service" ''
          [Unit]
          Description=uplink — mail, Matrix, calendar and tasks in one web client
          Documentation=file://${self}/README.md
          Wants=network-online.target
          After=network-online.target

          [Service]
          Type=exec
          ExecStart=${pkgs.lib.getExe uplinkBin} --config %E/uplink/uplink.yaml serve
          StateDirectory=uplink
          Restart=on-failure
          RestartSec=5

          # --- Sandbox ---------------------------------------------------------
          # It parses mail from anyone and fetches the images that mail links
          # to, so it is assumed to be reachable.
          #
          # The home directory is replaced with an empty tmpfs and only what the
          # service uses is mounted back: ProtectSystem=strict alone leaves
          # /home, its own mount, read-write.
          ProtectSystem=strict
          ProtectHome=tmpfs
          BindReadOnlyPaths=%E/uplink
          BindPaths=%S/uplink

          NoNewPrivileges=true
          PrivateTmp=true
          PrivateDevices=true
          ProtectProc=invisible
          ProtectHostname=true
          ProtectClock=true
          ProtectKernelTunables=true
          ProtectKernelModules=true
          ProtectKernelLogs=true
          ProtectControlGroups=true
          RestrictNamespaces=true
          RestrictRealtime=true
          RestrictSUIDSGID=true
          LockPersonality=true
          MemoryDenyWriteExecute=true
          CapabilityBoundingSet=
          SystemCallArchitectures=native
          SystemCallFilter=@system-service
          RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
          UMask=0077

          [Install]
          WantedBy=default.target
        '';

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
          inherit emulate service;
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
