# The web UI in Chromium, driven with the keyboard against uplink,
# emulated Gmail, and a Conduit homeserver (SPEC §15.10).
{ pkgs, uplinkModule, emulate }:

let
  machine = import ./gmail-machine.nix { inherit pkgs uplinkModule emulate; };

  uplinkUi = pkgs.writers.writePython3Bin "uplink-ui"
    {
      libraries = [ pkgs.python3Packages.playwright ];
      flakeIgnore = [ "E501" ];
    }
    (builtins.readFile ./e2e_ui.py);
in
pkgs.testers.nixosTest {
  name = "uplink-e2e";

  nodes.machine = {
    imports = [ machine.module ];
    environment.systemPackages = [ uplinkUi ];

    services.matrix-conduit = {
      enable = true;
      settings.global = {
        server_name = "localhost";
        address = "127.0.0.1";
        allow_registration = true;
        allow_federation = false;
        trusted_servers = [ ];
      };
    };
    services.uplink.settings.accounts = [
      {
        id = "alice-matrix";
        kind = "matrix";
        homeserver = "http://127.0.0.1:6167";
        mxid = "@alice:localhost";
      }
    ];
    environment.variables = {
      PLAYWRIGHT_BROWSERS_PATH = "${pkgs.playwright-driver.browsers}";
      PLAYWRIGHT_SKIP_VALIDATE_HOST_REQUIREMENTS = "true";
    };
    fonts.packages = [ pkgs.dejavu_fonts ];
    virtualisation.memorySize = 3072;
    virtualisation.cores = 2;
  };

  testScript = machine.testScriptPrefix + builtins.readFile ./e2e_test.py;
}
