# The README's screenshots: uplink in Chromium, light and dark, over a
# demo of a personal Gmail account, a work Microsoft 365 account with its
# calendar, and Matrix rooms, all emulated in a VM. The output has them
# in docs/screenshots/.
#
#   nix build .#screenshots
{ pkgs, uplinkModule, emulate }:

let
  machine = import ../tests/gmail-machine.nix { inherit pkgs uplinkModule emulate; };

  microsoftPort = 4003;
  microsoft = "http://127.0.0.1:${toString microsoftPort}";
  microsoftConfig = (pkgs.formats.yaml { }).generate "emulate-microsoft.yaml" {
    microsoft.users = [{ email = "alice@example.org"; name = "Alice"; }];
  };

  capture = pkgs.writers.writePython3Bin "uplink-shots"
    {
      libraries = [ pkgs.python3Packages.playwright ];
      flakeIgnore = [ "E501" ];
    }
    (builtins.readFile ./capture.py);

  # The networks' icons, as their mautrix bridges ship them, which the
  # bridges give their spaces and bots.
  bridgeIcons = pkgs.runCommand "bridge-icons" { nativeBuildInputs = [ pkgs.librsvg ]; } (
    pkgs.lib.concatStrings (pkgs.lib.mapAttrsToList
      (bridge: { rev, hash }: ''
        mkdir -p $out
        rsvg-convert --width 128 --height 128 --keep-aspect-ratio --output $out/${bridge}.png ${pkgs.fetchurl {
          url = "https://raw.githubusercontent.com/mautrix/${bridge}/${rev}/.idea/icon.svg";
          inherit hash;
        }}
      '')
      {
        whatsapp = { rev = "015ad733a3ef8a8776266b0c490ef6c740fe2ed8"; hash = "sha256-O4AKfdZ/7Ksktef1+wV3dAJaUwVn+FXO09qG+OEv+aU="; };
        telegram = { rev = "a36e68b399a3b014c1365f5c38ab05e8749080c0"; hash = "sha256-MCu12d/UHP5t9+TokmP1hlQa6I+bZ3e5plwvpQ07B0U="; };
        signal = { rev = "a96f214609cbe04426c637845902bc2fa5a0c537"; hash = "sha256-GoWErFg9/arf/N2M9n+EiUe9m3HaNSgg2UIgfWy4ZkE="; };
        slack = { rev = "6ffd02f161fc38c76a2fe50428fca5f6b980592c"; hash = "sha256-FxYEd6R1FmrQVW4AKOOwvQY01wwC57kgwEpEKeoYSrM="; };
      }));
in
pkgs.testers.nixosTest {
  name = "uplink-screenshots";

  nodes.machine = {
    imports = [ machine.module ];
    environment.systemPackages = [ capture ];

    systemd.services.emulate-microsoft = {
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        ExecStart = "${emulate}/bin/emulate start --service microsoft --port ${toString microsoftPort} --config ${microsoftConfig}";
        DynamicUser = true;
        StateDirectory = "emulate-microsoft";
        WorkingDirectory = "/var/lib/emulate-microsoft";
      };
    };

    services.matrix-conduit = {
      enable = true;
      settings.global = {
        server_name = "example.org";
        address = "127.0.0.1";
        allow_registration = true;
        allow_federation = false;
        trusted_servers = [ ];
      };
    };

    # Alice's office is in Berlin; her colleagues' meetings, in New York.
    services.uplink.settings.calendar.time_zones = [
      { label = "Berlin"; zone = "Europe/Berlin"; }
      { label = "NY"; zone = "America/New_York"; }
    ];
    services.uplink.settings.accounts = [
      {
        id = "alice-work";
        kind = "m365";
        address = "alice@example.org";
        oauth = {
          client_id = "uplink-demo";
          authority = "${microsoft}/common";
          resource = microsoft;
        };
      }
      {
        id = "alice-matrix";
        kind = "matrix";
        homeserver = "http://127.0.0.1:6167";
        mxid = "@alice:example.org";
      }
    ];

    environment.variables = {
      PLAYWRIGHT_BROWSERS_PATH = "${pkgs.playwright-driver.browsers}";
      PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS = "true";
    };
    fonts.packages = [ pkgs.roboto pkgs.noto-fonts-color-emoji pkgs.dejavu_fonts ];
    virtualisation.memorySize = 3072;
    virtualisation.cores = 2;
  };

  testScript = machine.testScriptPrefix + ''
    MICROSOFT = "${microsoft}"
    BRIDGE_ICONS = "${bridgeIcons}"
  '' + builtins.readFile ./seed.py;
}
