# End-to-end test of Gmail sync (SPEC §14 M4): uplink as a systemd service,
# next to vercel-labs/emulate as the Gmail and Google OAuth fake, driven
# over the WebSocket protocol.
{ pkgs, uplinkModule, emulate }:

let
  emulatePort = 4002;
  google = "http://127.0.0.1:${toString emulatePort}";

  certs = pkgs.runCommand "uplink-test-cert" { nativeBuildInputs = [ pkgs.openssl ]; } ''
    mkdir $out
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
      -subj /CN=localhost -addext subjectAltName=DNS:localhost -days 3650 \
      -keyout $out/key.pem -out $out/cert.pem
  '';

  emulateConfig = (pkgs.formats.yaml { }).generate "emulate.yaml" {
    google = {
      users = [{ email = "alice@example.com"; name = "Alice"; }];
      oauth_clients = [
        {
          client_id = "uplink-test";
          client_secret = "uplink-test-secret";
          redirect_uris = [ "http://127.0.0.1/" ];
        }
      ];
    };
  };

  uplinkWs = pkgs.writers.writePython3Bin "uplink-ws"
    {
      libraries = [ pkgs.python3Packages.websockets ];
      flakeIgnore = [ "E501" ];
    }
    (builtins.readFile ./uplink_ws.py);
in
pkgs.testers.nixosTest {
  name = "uplink-gmail";

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
        master_key_file = "/var/lib/uplink/master.key";
        accounts = [
          {
            id = "alice-gmail";
            kind = "gmail";
            address = "alice@example.com";
            oauth = {
              client_id = "uplink-test";
              client_secret = "uplink-test-secret";
              token_file = "/var/lib/uplink/accounts/alice-gmail.token";
            };
            endpoints = {
              auth = "${google}/o/oauth2/v2/auth";
              token = "${google}/oauth2/token";
              api = google;
            };
          }
        ];
      };
    };
    # Started by the test once it has a master key and a token.
    systemd.services.uplink.wantedBy = lib.mkForce [ ];

    systemd.services.emulate = {
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        ExecStart = "${emulate}/bin/emulate start --service google --port ${toString emulatePort} --config ${emulateConfig}";
        DynamicUser = true;
        StateDirectory = "emulate";
        WorkingDirectory = "/var/lib/emulate";
      };
    };

    environment.systemPackages = [ uplinkWs pkgs.curl pkgs.jq pkgs.sqlite ];
    environment.variables = {
      UPLINK_URL = "wss://localhost:8443/ws";
      UPLINK_CA = "${certs}/cert.pem";
      UPLINK_USER = "alice";
      UPLINK_PASSWORD = "testpass";
    };
    virtualisation.memorySize = 2048;
  };

  testScript = ''
    GOOGLE = "${google}"
  '' + builtins.readFile ./gmail_test.py;
}
