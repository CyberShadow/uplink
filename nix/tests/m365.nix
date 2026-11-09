# End-to-end test of Microsoft 365 sync (SPEC §14 M5–M6), driven over the
# WebSocket protocol, against emulate's Microsoft Graph and Azure AD.
{ pkgs, uplinkModule, emulate }:

let
  emulatePort = 4003;
  microsoft = "http://127.0.0.1:${toString emulatePort}";

  certs = pkgs.runCommand "uplink-test-cert" { nativeBuildInputs = [ pkgs.openssl ]; } ''
    mkdir $out
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
      -subj /CN=localhost -addext subjectAltName=DNS:localhost -days 3650 \
      -keyout $out/key.pem -out $out/cert.pem
  '';

  # No OAuth clients: the Outlook client is public, and any client id is
  # accepted.
  emulateConfig = (pkgs.formats.yaml { }).generate "emulate.yaml" {
    microsoft.users = [
      { email = "alice@example.com"; name = "Alice"; }
      { email = "bob@example.com"; name = "Bob"; }
    ];
  };

  uplinkWs = pkgs.writers.writePython3Bin "uplink-ws"
    {
      libraries = [ pkgs.python3Packages.websockets ];
      flakeIgnore = [ "E501" ];
    }
    (builtins.readFile ./uplink_ws.py);
in
pkgs.testers.nixosTest {
  name = "uplink-m365";

  nodes.machine = { lib, ... }: {
    imports = [ uplinkModule ];

    services.uplink = {
      enable = true;
      settings = {
        server = {
          bind = "127.0.0.1:8443";
          tls = { cert = "${certs}/cert.pem"; key = "${certs}/key.pem"; };
          # password: testpass
          basic_auth = {
            user = "alice";
            password_bcrypt = "$2y$10$f6FV2hxq2bDQpdC9Agjf3.41Cw0OIwOPjFgnboZw0NFMP1hkv6Uwa";
          };
        };
        send_delay_secs = 2;
        accounts = [
          {
            id = "alice-m365";
            kind = "m365";
            address = "alice@example.com";
            oauth = {
              client_id = "d3590ed6-52b3-4102-aeff-aad2292ab01c";
              authority = "${microsoft}/common";
              resource = microsoft;
            };
          }
        ];
      };
    };
    # Started by the test, once it has a token.
    systemd.services.uplink.wantedBy = lib.mkForce [ ];

    systemd.services.emulate = {
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        ExecStart = "${emulate}/bin/emulate start --service microsoft --port ${toString emulatePort} --config ${emulateConfig}";
        DynamicUser = true;
        StateDirectory = "emulate";
        WorkingDirectory = "/var/lib/emulate";
      };
    };

    environment.systemPackages = [ pkgs.curl pkgs.sqlite uplinkWs ];
    environment.variables = {
      UPLINK_URL = "https://localhost:8443";
      UPLINK_CA = "${certs}/cert.pem";
      UPLINK_USER = "alice";
      UPLINK_PASSWORD = "testpass";
    };
    virtualisation.memorySize = 2048;
  };

  testScript = ''
    MICROSOFT = "${microsoft}"
  '' + builtins.readFile ./m365_test.py;
}
