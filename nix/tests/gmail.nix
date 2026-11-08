# End-to-end test of Gmail sync (SPEC §14 M4), driven over the WebSocket
# protocol by a small client standing in for the UI.
{ pkgs, uplinkModule, emulate }:

let
  machine = import ./gmail-machine.nix { inherit pkgs uplinkModule emulate; };

  uplinkWs = pkgs.writers.writePython3Bin "uplink-ws"
    {
      libraries = [ pkgs.python3Packages.websockets ];
      flakeIgnore = [ "E501" ];
    }
    (builtins.readFile ./uplink_ws.py);
in
pkgs.testers.nixosTest {
  name = "uplink-gmail";

  nodes.machine = {
    imports = [ machine.module ];
    environment.systemPackages = [ uplinkWs ];
    virtualisation.memorySize = 2048;
  };

  testScript = machine.testScriptPrefix + builtins.readFile ./gmail_test.py;
}
