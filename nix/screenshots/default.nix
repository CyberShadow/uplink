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

    services.uplink.settings.accounts = [
      {
        id = "alice-work";
        kind = "m365";
        address = "alice@example.org";
        oauth = {
          client_id = "uplink-demo";
          redirect_uri = "http://127.0.0.1/redirect";
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
  '' + builtins.readFile ./seed.py;
}
