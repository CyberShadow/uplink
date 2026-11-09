# NixOS module running uplink as a systemd service.
#
#   services.uplink = {
#     enable = true;
#     settings = { server = { ... }; accounts = [ ... ]; };  # uplink.yaml
#   };
#
# The configuration is written to /etc/uplink/uplink.yaml, where the CLI
# finds it too: `sudo -u uplink uplink --config /etc/uplink/uplink.yaml
# oauth init --account ...`. It ends up in the Nix store, world-readable,
# so keep secrets out of it: the master key and tokens are in the data
# directory, and the TLS key (if uplink terminates TLS) in a file it
# points to.
{ defaultPackage }:
{ config, lib, pkgs, ... }:

let
  cfg = config.services.uplink;
  format = pkgs.formats.yaml { };
in
{
  options.services.uplink = {
    enable = lib.mkEnableOption "uplink, a self-hosted personal communication hub";

    package = lib.mkOption {
      type = lib.types.package;
      default = defaultPackage pkgs;
      defaultText = lib.literalMD "uplink from this flake";
      description = "The uplink package.";
    };

    settings = lib.mkOption {
      type = format.type;
      description = ''
        Contents of uplink.yaml (docs/SPEC.md §3). `data_dir` defaults
        to the service's state directory.
      '';
    };

    configFile = lib.mkOption {
      type = lib.types.path;
      readOnly = true;
      default = "/etc/uplink/uplink.yaml";
      description = "Where the configuration is installed.";
    };
  };

  config = lib.mkIf cfg.enable {
    services.uplink.settings.data_dir = lib.mkDefault "/var/lib/uplink";

    environment.etc."uplink/uplink.yaml".source = format.generate "uplink.yaml" cfg.settings;
    environment.systemPackages = [ cfg.package ];

    users.users.uplink = {
      isSystemUser = true;
      group = "uplink";
      home = "/var/lib/uplink";
    };
    users.groups.uplink = { };

    systemd.services.uplink = {
      description = "uplink";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      restartTriggers = [ config.environment.etc."uplink/uplink.yaml".source ];
      serviceConfig = {
        ExecStart = "${lib.getExe cfg.package} --config ${cfg.configFile} serve";
        User = "uplink";
        Group = "uplink";
        StateDirectory = "uplink";
        StateDirectoryMode = "0700";
        Restart = "on-failure";
        RestartSec = 5;
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
      };
    };
  };
}
