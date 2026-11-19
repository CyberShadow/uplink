# matrix-sdk-base 0.19.1, patched

The crate as published (crates.io 0.19.1, matrix-rust-sdk
`b18166c68bb958a21f0bca8b2d8320cb53583362`, `crates/matrix-sdk-base`):
its `src`, `Cargo.toml` (as normalized for publishing) and `README.md`.
Uplink's changes to it are the commits to this directory after the one
adding it (`git log vendor/matrix-sdk-base`).

Drop it, and its `[patch.crates-io]` entry in the workspace's
`Cargo.toml`, once a release has the fixes.

## Fixes

- `StateChanges::add_receipts` merges a room's receipt events, rather
  than keeping the last: a sync may have several (Conduit and tuwunel
  send one per user), and all but the last of a sync were lost. Unfixed
  upstream as of 2026-11-19. Uplink's test:
  `receipts_of_one_sync_are_all_stored`.
