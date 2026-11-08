# uplink

A self-hosted, single-user web client for mail (Gmail now; Microsoft 365
next), Matrix, calendar and tasks: one HTTPS + WebSocket server, a
keyboard-driven web UI with Gmail's shortcuts, and a transaction log that
makes every action undoable. See `docs/VISION.md` for the goals and
`docs/SPEC.md` for the design.

**Status:** Gmail mail can be read, searched, archived, labelled, starred
and marked read/unread, with undo, and written (drafts saved as you type,
synced across tabs), replied to, forwarded and sent, with undo-send.
Matrix rooms (end-to-end encrypted ones too) can be read and written to,
in plain text. Microsoft 365, calendar and tasks are in progress
(SPEC §14).

## Try it

You need [Nix](https://nixos.org/download) with flakes.

```sh
nix build                      # result/bin/uplink, with the web UI built in
cp examples/uplink.yaml uplink.yaml
./result/bin/uplink keygen > master.key && chmod 600 master.key
```

Then set up a Gmail account (next section), and:

```sh
./result/bin/uplink oauth init --account alice-gmail   # once per account
./result/bin/uplink serve
```

Open <https://localhost:8443/> and accept the certificate warning: the
example uses a public, self-signed development certificate
(`examples/dev-cert.pem`). Sign in as `alice` with the password `uplink`.
To choose your own password, put the output of
`echo 'your password' | ./result/bin/uplink hash-password` in
`server.basic_auth.password_bcrypt`.

`uplink` reads `uplink.yaml` in the current directory; pass `--config PATH`
otherwise. Data goes to `data.root` (`./data` in the example).

Press `?` in the UI for the keyboard shortcuts.

## Connecting a Gmail account

Gmail access goes through an OAuth client that you register yourself
(SPEC §15.2); it takes about ten minutes, once.

1. In the [Google Cloud console](https://console.cloud.google.com/), create
   a project (any name).
2. Enable the **Gmail API** for it (APIs & Services → Library).
3. Configure the **OAuth consent screen** (Google Auth Platform →
   Branding / Audience): user type *External*, and add your Gmail address
   under *Test users*.
4. Create an **OAuth client** (Google Auth Platform → Clients → Create
   client) of type *Desktop app*. Copy its client id and secret into the
   account's `oauth` section in `uplink.yaml`, and set `address` and `id`.
5. Run `uplink oauth init --account ID`, open the URL it prints, sign in,
   and accept. Google warns that the app is unverified: it is your own
   client, so continue. uplink receives the authorization on a local port,
   stores the refresh token sealed with the master key, and prints
   `Authorized`. If the browser runs on another machine than uplink, its
   final redirect to `http://127.0.0.1:…` fails to load: copy that URL from
   the address bar and paste it into `uplink oauth init`.
6. Start (or restart) `uplink serve`; the first sync mirrors the mailbox,
   then changes arrive within seconds.

While the consent screen's publishing status is *Testing*, Google expires
refresh tokens after 7 days, and `oauth init` must be run again. Setting
the status to *In production* (Audience → Publish app) avoids that; for a
client only you use, Google does not require verification, though the
consent screen keeps warning that the app is unverified.

uplink asks for the `gmail.modify` scope: read mail, change labels and
send (it never deletes mail).

## Connecting a Matrix account

Add the account to `uplink.yaml` (see the commented example), then pair
uplink with it as a new device:

```sh
./result/bin/uplink matrix pair --account alice-matrix
```

It asks for the account's password, then for its recovery key (or
Secret Storage passphrase): with it, uplink's device is verified, so your
other devices share encrypted rooms' keys with it. Leave it empty to
skip. A running uplink starts syncing the account within a minute.
Messages are plain text; other events show as their raw JSON.

## Running on NixOS

The flake provides a module that runs uplink as a systemd service:

```nix
{
  inputs.uplink.url = "path:/path/to/uplink";   # or a git URL

  outputs = { nixpkgs, uplink, ... }: {
    nixosConfigurations.host = nixpkgs.lib.nixosSystem {
      modules = [
        uplink.nixosModules.default
        {
          services.uplink = {
            enable = true;
            settings = {
              server = {
                bind = "0.0.0.0:8443";
                tls = { cert = "/var/lib/uplink/cert.pem"; key = "/var/lib/uplink/key.pem"; };
                basic_auth = { user = "alice"; password_bcrypt = "$2b$12$..."; };
              };
              master_key_file = "/var/lib/uplink/master.key";
              accounts = [{
                id = "alice-gmail";
                kind = "gmail";
                address = "alice@gmail.com";
                oauth = {
                  client_id = "....apps.googleusercontent.com";
                  client_secret = "GOCSPX-...";
                  token_file = "/var/lib/uplink/accounts/alice-gmail.token";
                };
              }];
            };
          };
        }
      ];
    };
  };
}
```

The configuration lands in the Nix store, world-readable, so keep secrets
(TLS key, master key, tokens) in the files it points to. Data lives in
`/var/lib/uplink` (the service's state directory). The service fails until
the master key exists; as root:

```sh
install -d -m 700 -o uplink -g uplink /var/lib/uplink   # if the service has not run yet
sudo -u uplink sh -c 'uplink keygen > /var/lib/uplink/master.key'
sudo -u uplink uplink --config /etc/uplink/uplink.yaml oauth init --account alice-gmail
systemctl restart uplink
```

Changing accounts in the configuration needs a restart; TLS and
basic-auth changes apply live.

## Development

```sh
nix develop                 # Rust, Node, pnpm, and UPLINK_EMULATE for the tests
nix flake check             # everything CI runs, including the NixOS VM tests
cargo test --workspace      # Rust tests (Gmail ones run against emulate)
cd web && pnpm install && pnpm test   # web tests
```

For working on the UI, run `uplink serve` (on the example's
`127.0.0.1:8443`) and `cd web && UPLINK_DEV_AUTH=alice:uplink pnpm dev`,
then open <http://localhost:5173/>. Vite proxies `/ws` to the server,
adding those credentials.

`web/src/generated/protocol.ts` is generated from `crates/uplink-protocol`;
after changing wire types, regenerate it with
`cargo run -q -p uplink-protocol --example typescript > web/src/generated/protocol.ts`.

### Testing against a real Gmail account

The Gmail tests normally run against
[emulate](https://github.com/vercel-labs/emulate), a Gmail API fake. To
run them against a real account instead (a throwaway one: the tests
deliver and delete messages, and clean up after themselves), set it up as
above in some `uplink.yaml`, then:

```sh
UPLINK_TEST_GMAIL_CONFIG=path/to/uplink.yaml UPLINK_TEST_GMAIL_ACCOUNT=ID \
  cargo test -p uplink-email --test gmail_sync
```
