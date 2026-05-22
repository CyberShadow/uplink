# uplink

Self-hosted, single-user personal communication hub: unified Gmail + M365 mail,
Matrix chat, M365 calendar, and local todos behind a single HTTPS+WebSocket
server. See `docs/SPEC.md` for the full technical specification.

## Prerequisites

- [Nix](https://nixos.org/download) with flakes enabled
- (Optional) [direnv](https://direnv.net/) for automatic shell activation

## Run from source

### 1. Enter the development shell

```sh
nix develop
```

This provides `rustc`, `cargo`, `pnpm`, `node`, and required system libraries.

### 2. Build the SPA

```sh
cd web && pnpm install && pnpm build && cd ..
```

This writes the static assets into `crates/uplink-server/static/`.

### 3. Build the binary

```sh
nix build
# result/bin/uplink is the output
```

Or for a quick dev build (no Nix, requires being inside `nix develop`):

```sh
cargo build --release
# target/release/uplink
```

### 4. Run

```sh
./result/bin/uplink --config examples/uplink.yaml
```

### 5. Open in a browser

Visit <https://localhost:8443/>.
Accept the self-signed certificate warning (dev fixture only).
Authenticate with:

- **Username:** `alice`
- **Password:** `uplink`

### 6. Run tests

```sh
nix flake check        # full check: cargo test + pnpm build
cargo test --workspace # Rust tests only
```

## Development cert

`examples/dev-cert.pem` and `examples/dev-key.pem` are a self-signed RSA-4096
certificate for `localhost` / `127.0.0.1`, valid for 10 years.

**These are public dev fixtures — never use them in production.**

To regenerate:

```sh
openssl req -x509 -newkey rsa:4096 -sha256 -days 3650 -nodes \
  -keyout examples/dev-key.pem \
  -out examples/dev-cert.pem \
  -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1"
```
