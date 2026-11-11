//! Prints the wire types as TypeScript, for `web/src/generated/protocol.ts`:
//!
//!     cargo run -q -p uplink-protocol --example typescript > web/src/generated/protocol.ts
//!
//! The `protocol-ts` flake check fails if the checked-in file is stale.

fn main() {
    print!("{}", uplink_protocol::typescript::typescript());
}
