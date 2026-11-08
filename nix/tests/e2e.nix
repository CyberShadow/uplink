# The web UI in Chromium, driven with the keyboard against uplink and
# emulated Gmail (SPEC §15.10).
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
