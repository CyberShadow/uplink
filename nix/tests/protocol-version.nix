# The protocol version uplink speaks, as the checked-in TypeScript says it
# (the `protocol-ts` check keeps that current), for uplink_ws.py's hello.
{ lib }:

let
  declaration = "export const PROTOCOL_VERSION = ";
  line = lib.findFirst (lib.hasPrefix declaration)
    (throw "protocol.ts declares no PROTOCOL_VERSION")
    (lib.splitString "\n" (builtins.readFile ../../web/src/generated/protocol.ts));
in
builtins.fromJSON (lib.removePrefix declaration line)
