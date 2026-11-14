<p align="center">
  <img alt="uplink logo" src="web/public/logo.svg" width="96">
</p>

<h1 align="center">uplink</h1>

<p align="center">
  <em>Your mail, chat, calendar and to-dos in one fast, keyboard-driven web app.<br>
  Self-hosted, and every action undoable.</em>
</p>

---

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://files.cy.md/uplink/docs/screenshots/inbox-dark.png">
    <source media="(prefers-color-scheme: light)" srcset="https://files.cy.md/uplink/docs/screenshots/inbox-light.png">
    <img alt="All inboxes: personal and work mail in one list" src="https://files.cy.md/uplink/docs/screenshots/inbox-light.png" width="800">
  </picture>
</p>

## What is this?

uplink is one web app for your **mail**, your **chats** and your
**calendar**, plus a to-do list of its own. It runs on your own server,
for one person: you. You open it in a browser, on your desktop or your
phone, and everything you do in one tab shows up in every other at once.

It is built for people who live in their inbox and want it fast:

- **One inbox for all your mail.**
  Your personal and work accounts side by side, or one at a time. Each
  message says whose it is; replies go out from the right account.
  Labels, folders and categories, stars, read and unread, archive, trash
  and spam all work as they do at your provider, and changes made
  elsewhere show up without a reload.

- **Gmail's keyboard shortcuts, everywhere.**
  `j`/`k` to move, `e` to archive, `r` to reply, `l` to label, `g i` to go
  to the Inbox, and the rest of Gmail's scheme, in mail and beyond: `g c`
  for the calendar, `g k` for to-dos, `Ctrl+K` to jump to any view or
  room. Press `?` for the full list.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://files.cy.md/uplink/docs/screenshots/conversation-dark.png">
    <source media="(prefers-color-scheme: light)" srcset="https://files.cy.md/uplink/docs/screenshots/conversation-light.png">
    <img alt="A conversation" src="https://files.cy.md/uplink/docs/screenshots/conversation-light.png" width="800">
  </picture>
</p>

- **Undo anything.**
  Every change you make is recorded, and the History (`g z`) undoes any of
  it, not just the last: an archive from yesterday, a label applied to
  fifty conversations, a room you left. Sent mail waits a few seconds in
  the outbox, so sending can be undone too.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://files.cy.md/uplink/docs/screenshots/history-dark.png">
    <source media="(prefers-color-scheme: light)" srcset="https://files.cy.md/uplink/docs/screenshots/history-light.png">
    <img alt="The history: every change, each with its Undo" src="https://files.cy.md/uplink/docs/screenshots/history-light.png" width="800">
  </picture>
</p>

- **Writing that never loses a word.**
  Drafts are saved as you type, on the server, and stay in step across
  tabs and devices: start a reply on your laptop, finish it on your phone.
  Rich text, attachments, reply, reply to all and forward, with the quoted
  mail folded away.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://files.cy.md/uplink/docs/screenshots/compose-dark.png">
    <source media="(prefers-color-scheme: light)" srcset="https://files.cy.md/uplink/docs/screenshots/compose-light.png">
    <img alt="Writing a reply" src="https://files.cy.md/uplink/docs/screenshots/compose-light.png" width="800">
  </picture>
</p>

- **One search for everything.**
  A search finds your mail, your chats, your to-dos and your calendar's
  events, in one list, the most recently active first. A chat shows up
  once, with how many of its messages match and the latest of them, and
  opens on those messages. Select mail and chats together to mark them
  all read or star them at once.

- **Search that speaks Gmail.**
  `from:`, `to:`, `subject:`, `label:`, `is:unread`, `has:attachment`,
  `after:`, `older_than:`, `OR`, `-` and parentheses, across all your
  accounts at once, over a full-text index on your server, and of chats
  too (`from:`, `is:unread`, `is:mention`). Every list is a search, so
  any search can be bookmarked like a label.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://files.cy.md/uplink/docs/screenshots/search-dark.png">
    <source media="(prefers-color-scheme: light)" srcset="https://files.cy.md/uplink/docs/screenshots/search-light.png">
    <img alt="A search for &quot;trail&quot;: an email thread, a chat, a to-do and a calendar event, in one list" src="https://files.cy.md/uplink/docs/screenshots/search-light.png" width="800">
  </picture>
</p>

- **All your chats, through Matrix.**
  uplink is a full [Matrix](https://matrix.org/) client: rooms and direct
  chats, nested in their spaces, with end-to-end encryption (device
  verification by emoji, key backup and recovery), replies, threads,
  reactions, edits, files, link previews, pinned messages, mentions and
  notifications, and search, encrypted rooms included. With
  [bridges](https://matrix.org/ecosystem/bridges/) on your homeserver,
  your WhatsApp, Telegram, Signal and Slack chats are Matrix rooms
  too, so they are all here, in one place, beside your mail.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://files.cy.md/uplink/docs/screenshots/room-dark.png">
    <source media="(prefers-color-scheme: light)" srcset="https://files.cy.md/uplink/docs/screenshots/room-light.png">
    <img alt="A WhatsApp group, bridged to Matrix, among the other networks' chats" src="https://files.cy.md/uplink/docs/screenshots/room-light.png" width="800">
  </picture>
</p>

- **Your calendar, at a glance.**
  By day, week or as an agenda, recurring meetings and all; open a meeting
  for who is coming, its notes, and the link to join it; answer it, or
  plan your own, with its attendees, Teams link and recurrence.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://files.cy.md/uplink/docs/screenshots/calendar-dark.png">
    <source media="(prefers-color-scheme: light)" srcset="https://files.cy.md/uplink/docs/screenshots/calendar-light.png">
    <img alt="The calendar's week, a meeting opened" src="https://files.cy.md/uplink/docs/screenshots/calendar-light.png" width="800">
  </picture>
</p>

- **To-dos, and mail to follow up.**
  Keep a to-do list, and flag a message (`Shift+T`) to make a to-do of it
  that opens the message again.

- **On your phone too.**
  The layout becomes a phone app's when the window is narrow, and uplink
  can be installed to the home screen. Light and dark themes follow your
  system; HTML mail goes dark with them.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://files.cy.md/uplink/docs/screenshots/mobile-inbox-dark.png">
    <source media="(prefers-color-scheme: light)" srcset="https://files.cy.md/uplink/docs/screenshots/mobile-inbox-light.png">
    <img alt="The inbox on a phone" src="https://files.cy.md/uplink/docs/screenshots/mobile-inbox-light.png" height="400">
  </picture>
  &nbsp;&nbsp;
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://files.cy.md/uplink/docs/screenshots/mobile-room-dark.png">
    <source media="(prefers-color-scheme: light)" srcset="https://files.cy.md/uplink/docs/screenshots/mobile-room-light.png">
    <img alt="A Telegram group, bridged to Matrix, on a phone" src="https://files.cy.md/uplink/docs/screenshots/mobile-room-light.png" height="400">
  </picture>
</p>

- **Yours.**
  uplink keeps its copy of your mail and chats on your server, and talks
  to the providers with your own credentials. Sign-in tokens are sealed
  with a key only your server has. HTML mail is sanitized and sandboxed,
  and its remote images, loaded only when you ask, are fetched by the
  server rather than by your browser.

## Status

uplink is young. What it connects to so far:

|          | Supported                                   | Planned                                    |
|----------|---------------------------------------------|--------------------------------------------|
| Mail     | Gmail, Microsoft 365, any IMAP, POP3 or JMAP server |                                    |
| Calendar | Microsoft 365                               | Google Calendar, CalDAV, iCalendar feeds   |
| Chat     | Matrix, and every network bridged to it     |                                            |
| To-dos   | uplink's own                                | Google Tasks, Microsoft To Do, CalDAV      |

It is for **one user per server**, by design. Signing in is HTTP basic
authentication, so put uplink behind TLS (or keep it on loopback). An
Android app is planned too; see [`docs/ROADMAP.md`](docs/ROADMAP.md) for
the rest.

## Getting started

You need [Nix](https://nixos.org/download) with flakes.

```sh
nix build                      # result/bin/uplink, with the web UI built in
./result/bin/uplink init       # writes uplink.yaml, asking for a user and password
```

`uplink init` writes `uplink.yaml` (a copy of `examples/uplink.yaml`,
with your user and password hash) and makes its data directory, `data`,
with the master key that seals accounts' tokens. Add your accounts to it
(the next sections), and:

```sh
./result/bin/uplink oauth init --account alice-gmail   # once per account
./result/bin/uplink serve
```

Open <http://localhost:8443/> and sign in. The server listens on
loopback only, without TLS, as `init` configures it; to reach it from
elsewhere, put a reverse proxy that terminates TLS in front of it (e.g.
Caddy: `reverse_proxy 127.0.0.1:8443`; it must pass on the `Host` asked
for, which Caddy does and nginx does with `proxy_set_header Host $host`),
or give it a certificate of its own (`server.tls`). To change the password, put the output of
`echo 'new password' | ./result/bin/uplink hash-password` in
`server.basic_auth.password_bcrypt`.

`uplink` reads `uplink.yaml` in the current directory; pass `--config PATH`
otherwise. Relative paths in it are relative to its directory.

### Connecting a Gmail account

Gmail access goes through an OAuth client that you register yourself; it
takes about ten minutes, once.

1. In the [Google Cloud console](https://console.cloud.google.com/), create
   a project (any name).
2. Enable the **Gmail API** and the **Cloud Pub/Sub API** for it (APIs &
   Services → Library), or run `gcloud services enable
   gmail.googleapis.com pubsub.googleapis.com --project PROJECT`. uplink
   sets up the rest of what Gmail's push notifications need in the
   project itself (a topic and a subscription named `uplink-ID`).
3. Configure the **OAuth consent screen** (Google Auth Platform →
   Branding / Audience): user type *External*, and add your Gmail address
   under *Test users*.
4. Create an **OAuth client** (Google Auth Platform → Clients → Create
   client) of type *Desktop app*. In `uplink.yaml`, uncomment the Gmail
   account (dropping the `[]` after `accounts:`), set `id` (any name) and
   `address`, and copy the client's id and secret into its `oauth`, and
   the project's ID (on the console's dashboard, or in its project
   picker) as its `project`.
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
send; and for `pubsub`, to make the topic Gmail tells of changes on, and
read from it. It never deletes mail itself: `#` moves it to Trash, which
Gmail empties after 30 days. An account authorized before uplink asked
for `pubsub` is polled every 5 s, saying in the log why it has no push
notifications, until `uplink oauth init` is run again.

### Connecting a Microsoft 365 account

Microsoft 365 access goes through an app registration in Microsoft Entra
ID that you (or your organization's administrator) create.

1. In the [Microsoft Entra admin center](https://entra.microsoft.com/),
   under App registrations, register a new application (any name), for
   accounts in your organization's directory.
2. Under Authentication, add the platform *Mobile and desktop
   applications* with the redirect URI
   `https://login.microsoftonline.com/common/oauth2/nativeclient`.
3. Under API permissions, add the Microsoft Graph delegated permissions
   `Mail.ReadWrite`, `Mail.Send` and `Calendars.ReadWrite` (to answer
   meeting requests, and to create and edit events). If your
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

### Connecting any other mail account (IMAP or POP3, and SMTP)

In `uplink.yaml`, uncomment the IMAP account and set `address` and the
two servers' `host`. They are reached over TLS on ports 993 (IMAP) and
465 (SMTP); add `security: starttls` to a server reached by STARTTLS
instead (ports 143 and 587), or `port` for another port, and `user` if
the servers know you by a name other than your address. Then set the
password, which uplink seals in its data directory (an app password, if
your provider gives those for mail clients):

```sh
./result/bin/uplink password --account alice-mail
```

For a provider that signs mail clients in with OAuth (XOAUTH2) instead,
give the account an `oauth` block of your own OAuth client (`client_id`,
`client_secret` if it has one, `auth_url`, `token_url` and `scope`, as
the provider documents them; the scope must yield a refresh token) and
run `uplink oauth init --account alice-mail`.

New mail and other changes show as they happen in every folder if the
server has NOTIFY (Dovecot does), else in the Inbox only, other folders
being checked every 30 s. To have more folders watched on such a server,
name them in `watch` (e.g. `watch: [Alerts, Lists/rust]`, by their path
on the server). Each takes a connection of its own, of the few a server
allows a user at once.

Folders are labels of which a message has one; the server's Sent,
Drafts, Trash, Junk and Archive folders are uplink's, and an Archive is
made if there is none. Read and starred are the `\Seen` and `\Flagged`
flags, and other keywords are labels. New mail, and changes to the
Inbox, arrive at once if the server has IDLE (most do); other changes
made in other mail clients, within 30 s. Sent mail goes by SMTP, and
uplink files the copy in Sent itself.

A server reached by POP3 instead takes the POP3 account of the example
(`kind: pop3`, its `pop3` server on port 995, or 110 by STARTTLS), set
up the same way. uplink then downloads the mail, within a minute of its
arrival, and keeps its labels itself: nothing you do to it in uplink
reaches the server. The mail is left on the server for other clients,
unless the account says `delete_after_download: true`.

### Connecting a JMAP account (Stalwart, Fastmail, …)

A server that speaks JMAP takes the JMAP account of the example (`kind:
jmap`), with its `session_url`: for most servers
`https://<host>/.well-known/jmap`. Set the password as for an IMAP
account (`uplink password --account alice-jmap`), or give it an `oauth`
block and run `uplink oauth init`.

Mailboxes are labels, as in Gmail: a message can be in several. But
Trash, Junk and the Archive are folders, as JMAP has them: trashing a
message leaves it in Trash alone, so other clients don't show it in the
Inbox, and undo puts it back where it was. Archiving takes a message
out of the Inbox, and one left in no mailbox goes into the Archive
(made if there is none). Read and starred are the `$seen` and
`$flagged` keywords, and other keywords are labels. Changes arrive as they happen, as the server tells of them.
Sent mail goes by the server's own submission, which files the copy in
Sent.

### Sending from more than one address

A mail account sends from its `address`, and from any of its
`identities`: addresses, and domains any address at which is yours
(handy if you give everyone an address of their own). Each is sent
under the account's `name`, unless it has a `name` of its own:

```yaml
  - id: alice-jmap
    kind: jmap
    address: alice@example.com
    name: Alice Liddell
    identities:
      - domain: example.com
      - address: alice@example.org
        name: "Alice (Example.org)"
```

All of them are you: `me` in searches (`from:me`) and in conversation
lists, and left out when replying to all. The compose window's From
offers them, and the server refuses to send from any other address.
Whether the provider sends from an address is the provider's: Gmail,
only from its send-as aliases (Settings → Accounts → Send mail as),
and uplink refuses to send from any other rather than let Gmail send
it from your Gmail address; a mail server, from what it lets the
account send from (Stalwart: its addresses and aliases, and for JMAP,
any address at their domains); Microsoft 365, from addresses you may
send as.

### Connecting a Matrix account

Add the account to `uplink.yaml` (see the commented example), then pair
uplink with it as a new device:

```sh
./result/bin/uplink matrix pair --account alice-matrix
```

It asks for the account's password, then for its recovery key (or
Secret Storage passphrase): with it, uplink's device is verified, so your
other devices share encrypted rooms' keys with it. Leave it empty to
skip; you can verify it later from the account's Security page, by
comparing emoji with another device. A running uplink starts syncing the
account within a minute.

### Running on NixOS

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

uplink is a Rust server (one process, one port: HTTPS and a WebSocket)
and a Preact web app. [`docs/VISION.md`](docs/VISION.md) has its goals,
and [`docs/SPEC.md`](docs/SPEC.md) its design.

```sh
nix develop                 # Rust, Node, pnpm, and the emulators for the tests
nix flake check             # everything CI runs, including the NixOS VM tests
cargo test --workspace      # Rust tests (against emulated Gmail and Microsoft 365, and Conduit)
cd web && pnpm install && pnpm test   # web tests
```

In `nix flake check`, each Rust test is a check of its own
(`backend-test-<crate>-<binary>-<test>`), run from test binaries built
once, so a retry reruns only the tests that failed.

For working on the UI, run `uplink serve` (on the example's
`127.0.0.1:8443`, or another at `UPLINK_DEV_SERVER=host:port`) and
`cd web && UPLINK_DEV_AUTH=alice:uplink pnpm dev`, then open
<http://localhost:5173/>. Vite proxies `/ws` to the server,
adding those credentials.

`web/src/generated/protocol.ts` is generated from `crates/uplink-protocol`;
after changing wire types, regenerate it with
`cargo run -q -p uplink-protocol --example typescript > web/src/generated/protocol.ts`.

The screenshots above are taken in a VM, of a demo seeded into emulated
providers: `nix build .#screenshots` puts them in
`result/docs/screenshots/`.

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

## License

uplink is licensed under the [Apache License 2.0](LICENSE). Its code was
written by AI (Claude, by Anthropic), directed and reviewed by a human.
