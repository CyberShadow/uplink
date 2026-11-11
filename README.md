# uplink

A self-hosted, single-user web client for mail (Gmail and Microsoft 365),
Matrix, calendar and tasks: one HTTPS + WebSocket server, a
keyboard-driven web UI with Gmail's shortcuts, and a transaction log that
makes every action undoable. See `docs/VISION.md` for the goals and
`docs/SPEC.md` for the design.

**Status:** Gmail and Microsoft 365 mail can be read by conversation, as
Gmail groups it (attachments too), searched,
archived, labelled, starred, marked read/unread, trashed and reported as
spam, with undo, and written (drafts saved as you type, synced across
tabs, with files attached), replied to, forwarded and sent, with
undo-send. Matrix rooms (end-to-end encrypted ones too) can be read,
written to in plain text, joined and left. To-dos can be kept, and
messages flagged for follow-up (which adds a to-do). The history lists
everything done, and undoes any of it. With several mail accounts, their
Inboxes are also one list. Microsoft 365 calendars are shown by week or
as an agenda (read-only). Microsoft 365 is tested against an emulator
only so far (SPEC §15.12).

## Try it

You need [Nix](https://nixos.org/download) with flakes.

```sh
nix build                      # result/bin/uplink, with the web UI built in
./result/bin/uplink init       # writes uplink.yaml, asking for a user and password
```

`uplink init` writes `uplink.yaml` (a copy of `examples/uplink.yaml`,
with your user and password hash) and makes its data directory, `data`,
with the master key that seals accounts' tokens. Add an account to it
(the next sections), and:

```sh
./result/bin/uplink oauth init --account alice-gmail   # once per account
./result/bin/uplink serve
```

Open <http://localhost:8443/> and sign in. The server listens on
loopback only, without TLS, as `init` configures it; to reach it from
elsewhere, put a reverse proxy that terminates TLS in front of it (e.g.
Caddy: `reverse_proxy 127.0.0.1:8443`), or give it a certificate of its
own (`server.tls`). To change the password, put the output of
`echo 'new password' | ./result/bin/uplink hash-password` in
`server.basic_auth.password_bcrypt`.

`uplink` reads `uplink.yaml` in the current directory; pass `--config PATH`
otherwise. Relative paths in it are relative to its directory.

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
   client) of type *Desktop app*. In `uplink.yaml`, uncomment the Gmail
   account (dropping the `[]` after `accounts:`), set `id` (any name) and
   `address`, and copy the client's id and secret into its `oauth`.
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
send. It never deletes mail itself: `#` moves it to Trash, which Gmail
empties after 30 days.

## Connecting a Microsoft 365 account

Microsoft 365 access goes through an app registration in Microsoft Entra
ID that you (or your organization's administrator) create (SPEC §13.3).

1. In the [Microsoft Entra admin center](https://entra.microsoft.com/),
   under App registrations, register a new application (any name), for
   accounts in your organization's directory.
2. Under Authentication, add the platform *Mobile and desktop
   applications* with the redirect URI
   `https://login.microsoftonline.com/common/oauth2/nativeclient`.
3. Under API permissions, add the Microsoft Graph delegated permissions
   `Mail.ReadWrite`, `Mail.Send` and `Calendars.Read`. If your
   organization does not let users consent to applications, an
   administrator grants them.
4. In `uplink.yaml`, uncomment the Microsoft 365 account, set `id` and
   `address`, and copy the registration's application (client) id into
   its `oauth.client_id`. A single-tenant registration also needs
   `oauth.authority: "https://login.microsoftonline.com/<tenant id>"`.

Then run:

```sh
./result/bin/uplink oauth init --account alice-m365
```

Open the URL it prints and sign in. The browser then lands on a blank
page at `…/oauth2/nativeclient?code=…`: copy that address, or the code
alone, and paste it into `uplink oauth init`. A running uplink picks
the account up once restarted; changes made in Outlook arrive within
30 s. Folders are labels of which a message has one; categories are
labels too. The account's calendar, from a month ago to a year ahead,
is under Calendar (`g c`), within 5 minutes of a change.

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
Invitations are joined or declined, and rooms left, from the room's
header.

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
              # Behind a reverse proxy terminating TLS; or add
              # tls = { cert = "/path/cert.pem"; key = "/path/key.pem"; };
              server = {
                bind = "127.0.0.1:8443";
                basic_auth = { user = "alice"; password_bcrypt = "$2b$12$..."; };
              };
              accounts = [{
                id = "alice-gmail";
                kind = "gmail";
                address = "alice@gmail.com";
                oauth = {
                  client_id = "....apps.googleusercontent.com";
                  client_secret = "GOCSPX-...";
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

The configuration lands in the Nix store, world-readable. The secrets
are not in it: the master key and accounts' tokens are in the data
directory, `/var/lib/uplink` (the service's state directory), and a TLS
key would be in a file it points to. Authorize each account, as root:

```sh
install -d -m 700 -o uplink -g uplink /var/lib/uplink   # if the service has not run yet
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
