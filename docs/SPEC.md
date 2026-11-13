# uplink — MVP technical specification

A self-hosted, single-user, web-based personal communication and productivity
tool. This document specifies the MVP cut targeted by `docs/ROADMAP.md`: one
Gmail account, one Microsoft 365 account (mail + read-only calendar), one
Matrix homeserver (plain text only), a local TODO list, a unified
transaction-log-backed undo system, and a Preact SPA frontend over a single-
process Rust backend.

**Audience:** the implementor (you). This is opinionated. Decisions are made
rather than enumerated. Anything left intentionally open is in §15.

**Companion documents:**

- `docs/VISION.md` — goals, non-goals.
- `docs/ROADMAP.md` — MVP scope by complement (i.e. what's *not* in MVP).

---

## Table of contents

1. [Overview & invariants](#1-overview--invariants)
2. [Repository layout](#2-repository-layout)
3. [Configuration](#3-configuration)
4. [Storage & schema](#4-storage--schema)
5. [Wire protocol (WebSocket)](#5-wire-protocol-websocket)
6. [Transaction log & undo](#6-transaction-log--undo)
7. [Drafts](#7-drafts)
8. [Email pipeline (Gmail, M365, IMAP, POP3, JMAP)](#8-email-pipeline-gmail-m365-imap-pop3-jmap)
9. [Matrix pipeline](#9-matrix-pipeline)
10. [Calendar pipeline](#10-calendar-pipeline)
11. [TODOs](#11-todos)
12. [Frontend](#12-frontend)
13. [Authentication & secrets](#13-authentication--secrets)
14. [Milestones](#14-milestones)
15. [Risks & open items](#15-risks--open-items)
16. [UI shell](#16-ui-shell)

Appendices:

- [A. Crate dependency summary](#appendix-a--crate-dependency-summary)
- [B. Code review checklist](#appendix-b--code-review-checklist)

---

## 1. Overview & invariants

### 1.1 Architecture at 30,000 ft

```
                        ┌──────────────────────────────────────────┐
                        │              uplink (one process)        │
                        │                                          │
   browser tab(s)       │  ┌─────────┐  ┌────────────────────────┐ │
   ─── WS/HTTPS ───────►│  │  axum   ├─►│  in-memory broadcast    │ │
                        │  │ rustls  │  │  hub (tokio::sync)      │ │
                        │  └────┬────┘  └──┬──────────────┬──────┘ │
                        │       │          │              │        │
                        │       ▼          ▼              ▼        │
                        │  ┌─────────────────────────────────────┐ │
                        │  │       op dispatcher (tx_log)        │ │
                        │  │  records → applies → broadcasts     │ │
                        │  └──┬───────────┬──────────┬──────┬────┘ │
                        │     │           │          │      │      │
                        │     ▼           ▼          ▼      ▼      │
                        │  Gmail        M365      Matrix   local   │
                        │  REST API    Graph    matrix-     todo   │
                        │  history     delta    rust-sdk    /draft │
                        │             ICS read                     │
                        │     │           │          │      │      │
                        │     └─────┬─────┴──────────┴──────┘      │
                        │           ▼                              │
                        │     ┌──────────────────────────┐         │
                        │     │ SQLite (WAL + FTS5)      │         │
                        │     │ + raw .eml on disk       │         │
                        │     │ + matrix sdk state store │         │
                        │     └──────────────────────────┘         │
                        └──────────────────────────────────────────┘
```

### 1.2 Hard invariants

These are the non-negotiable load-bearing decisions everything else hangs on.
Don't litigate these in code review.

1. **Single Rust binary, single port, single tokio runtime.** No daemon split,
   no sidecar, no separate sync worker. axum + tokio handles every connection:
   HTTP for the SPA, WebSocket for live data, plus async tasks for provider
   sync and reconciliation and the matrix-rust-sdk sync loop.
2. **Server is the trust boundary.** All credentials, all OAuth refresh
   tokens, all Matrix decryption keys, all message bodies live server-side in
   plaintext at rest (encryption of OAuth secrets is at-rest, with the key
   in a sibling file — see §13). Client receives already-decrypted, already-
   resolved, ready-to-render payloads.
3. **Every user-initiated state mutation is dispatched through the
   tx_log.** Single chokepoint. Every dispatch applies an op and records it
   with its inverse in one transaction. Ops change *desired* state locally;
   reconcilers bring provider state in line afterwards (§6.6). Mass
   operations are a *single* tx_log entry with a batch inverse. Undo is
   itself an op (append-only log).
4. **Raw `.eml` files are sacred.** Content-addressed by
   SHA-256(raw bytes), never modified, never deleted by the application, no
   attachment extraction. Indexes (search, threading, labels) live in SQLite
   and are *derived* and rebuildable from the on-disk blobs. (Calendar
   events are not files: Graph has no iCalendar export, and an event,
   unlike a message, changes. Each is kept as Graph's JSON in its row, a
   mirror that can be listed anew from Graph; §10.1.)
5. **Real-time uses one channel: a JSON-over-WebSocket protocol uplink
   defines.** No SSE, no long-poll, no second port. Heartbeat is app-level
   ping/pong at 25–30 s. The stream is the log itself (§6): reconnection
   replays the entries after the last one the client saw.
6. **No client-side persistence.** No client cache of mail metadata, no
   client cache of Matrix events. The SPA loads from the server on connect,
   keeps reactive state in memory, and writes nothing to disk. (IndexedDB
   write-through of draft text is deferred, §7.5.)

### 1.3 Out of scope for MVP (covered in `docs/ROADMAP.md`)

Quick references so the reader doesn't have to flip:

- All Matrix rich features (formatted body, reactions, replies, threads,
  edits, mentions, moderation, room settings). Plain text only; unknown
  event types render as raw JSON.
- E2EE on the client — server decrypts, client receives plaintext.
- SPA-side Matrix device-verification UI. Pairing is a CLI subcommand.
- SMTP send for Gmail, M365 and JMAP: Gmail sends via Gmail API, M365 via
  Graph, JMAP by `EmailSubmission`; SMTP serves IMAP and POP3 accounts
  only (§8.11).
- Drafts on the wire (only local drafts in MVP).
- Calendar write (M365 read-only). Google Calendar, CalDAV, ICS feeds: out.
- TODO backends other than local SQLite.
- ACME / Let's Encrypt automation — BYO cert + key, with hot-reload.

---

## 2. Repository layout

```
uplink/
├── Cargo.toml                       # workspace
├── crates/
│   ├── uplink-protocol/             # wire types; ts-rs codegen target
│   ├── uplink-config/               # YAML + inotify reload + section diff
│   ├── uplink-storage/              # rusqlite + migrations + .eml store
│   ├── uplink-tx-log/               # op dispatcher, Op trait, undo
│   ├── uplink-realtime/             # WebSocket protocol + broadcast hub
│   ├── uplink-query/                # the query language, of every kind
│   ├── uplink-email/                # Gmail API, M365 Graph mail; send
│   ├── uplink-matrix/               # matrix-rust-sdk wrapper
│   ├── uplink-calendar/             # M365 Graph calendar (read-only)
│   ├── uplink-todos/                # local to-dos and their ops
│   ├── uplink-server/               # axum, TLS, basic auth, routes
│   └── uplink-cli/                  # `uplink matrix pair`, `uplink migrate`
├── web/                             # Preact SPA (Vite)
│   ├── package.json
│   ├── vite.config.ts               # dev proxy to the backend
│   ├── src/
│   │   ├── main.tsx
│   │   ├── connection.ts            # WS manager + reconnect
│   │   ├── stores/                  # @preact/signals stores
│   │   ├── views/                   # Mail, Matrix, Calendar, TODO
│   │   ├── components/
│   │   └── generated/protocol.ts    # ts-rs output, checked in
│   └── tsconfig.json
├── docs/
│   ├── VISION.md
│   ├── ROADMAP.md
│   └── SPEC.md                      # this file
├── flake.nix                        # NixOS dev shell + buildable derivation
└── README.md
```

### 2.1 Why the split

- **`uplink-protocol`** is a leaf crate with no deps on the others. It owns
  every type that crosses the WS boundary, every type that crosses the SPA
  boundary, and derives `ts-rs` bindings for them: its `typescript`
  example prints `web/src/generated/protocol.ts`, which is checked in, and
  the `protocol-ts` flake check fails when it is stale. Keeps TS-side
  imports clean; nothing else can accidentally export a wire type.
- **`uplink-tx-log`** sits between callers (mail/matrix/todo handlers, undo
  endpoint, draft endpoint) and the storage layer. There's no second path
  into the database. Storage primitives can be called only via an op.
- **`uplink-realtime`** owns the broadcast hub and the WebSocket protocol.
  It is the only place that serializes server→client events. Callers push
  `ServerEvent` enum values into the hub; the realtime crate handles fan-out,
  buffering, and replay.
- **`uplink-server`** is the binary's entry point. It composes the others.
  It owns TLS config hot-reload (swap `Arc<rustls::ServerConfig>` on file
  change), basic-auth middleware, route registration, and the HTTP→WS upgrade
  path. It does *no* business logic.

### 2.2 Why not one big crate

Workspace crates compile incrementally; touching the Matrix wrapper doesn't
recompile the email pipeline. They also force you to be honest about which
module depends on which — `uplink-protocol` is a leaf, `uplink-tx-log`
sits below `uplink-email`/`-matrix`/`-todos`/`-calendar`, `uplink-server`
sits at the top. No cycles.

---

## 3. Configuration

### 3.1 File

Single YAML file. Path: `--config <path>` flag, default `./uplink.yaml`.
Hot-reloaded via `notify` crate (inotify on Linux). On change, the config
crate diffs the new vs old by section and pushes `ConfigChanged{section}`
events into the broadcast hub. Consumers (TLS, accounts, basic-auth) decide
whether to take effect immediately, lazily, or require restart.

### 3.2 Annotated example

`uplink init` writes `examples/uplink.yaml` (the reference, with every
account kind commented in it) with the user and password it asks for,
and makes the data directory with the master key. Only what cannot be
derived is configured: paths under the data directory are fixed (§4.1),
and Microsoft 365's sign-in is Entra ID's and Graph's unless set. Relative
paths are relative to the file's directory.

```yaml
server:
  bind: "127.0.0.1:8443"         # default
  # Optional: without it, plain HTTP, for a reverse proxy in front that
  # terminates TLS (§13.6).
  tls:
    cert: /etc/uplink/cert.pem    # PEM, full chain
    key:  /etc/uplink/key.pem     # PEM, encrypted is rejected
  basic_auth:
    user: alice
    # bcrypt hash with cost ≥10 (`uplink hash-password`). uplink rejects
    # plaintext.
    password_bcrypt: "$2b$12$xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"

# Everything uplink keeps (§4.1), including the master key and tokens.
data_dir: /var/lib/uplink

# How long sent mail waits in the outbox, during which undoing the send
# takes it back for sure (§6.7). Default 10.
send_delay_secs: 10

# The home view's buckets (§12.13), top to bottom, each a query (§8.7):
# an item is in the first that matches it; `badge` shows it on the app's
# icon (§12.14), by `count` or by a `dot`. Without `home`, these.
home:
  - name: Immediate
    query: is:mention OR is:invite OR (is:dm is:unread) OR is:overdue
      OR due:today OR (in:inbox is:unread is:important)
    badge: count
  - name: Upkeep
    query: (is:unread is:notifying -tag:lowpriority)
      OR (in:inbox -category:promotions -category:social)
    badge: dot
  - name: Backlog
    query: is:starred OR is:todo
  - name: Leisure
    query: is:unread

# The calendar (§10.2): its week's hours also in these time zones (IANA
# names), in columns left of the browser's own. None, without it.
calendar:
  time_zones:
    - label: Office
      zone: Europe/Berlin

accounts:
  # — Gmail — with the user's own Google Cloud project (Desktop app OAuth
  # client); scope gmail.modify (read, label, send; no permanent delete).
  - id: alice-gmail
    kind: gmail
    address: alice@gmail.com
    oauth:
      client_id:     "1234.apps.googleusercontent.com"
      client_secret: "GOCSPX-xxxxxxxxxx"

  # — Microsoft 365 (work) — with an Entra ID app registration of the
  # user's (or their organization's), a public client; v1 endpoint with
  # `resource=` (§13.3). `redirect_uri`, `authority` and `resource`
  # default to Entra ID's desktop redirect, the multi-tenant authority
  # and Graph; tests point them at a fake.
  - id: alice-m365
    kind: m365
    address: alice@example.com
    oauth:
      client_id: "11111111-2222-3333-4444-555555555555"

  # — Any mail server — IMAP and SMTP over TLS (`security: starttls` to
  # upgrade instead; ports default to 993 and 465, or 143 and 587), the
  # password sealed by `uplink password` (§13.4), or `oauth` (§13.7).
  - id: alice-mail
    kind: imap
    address: alice@example.org
    imap: { host: imap.example.org }
    smtp: { host: smtp.example.org }

  # — A mail server by POP3 (§8.12) — as IMAP, ports 995 or 110; mail is
  # left on the server unless `delete_after_download: true`.
  - id: alice-pop
    kind: pop3
    address: alice@example.net
    pop3: { host: pop.example.net }
    smtp: { host: smtp.example.net }

  # — A mail server by JMAP (§8.13) — its session resource, over https;
  # signed in as IMAP is (`user`, the password `uplink password` sealed,
  # or `oauth`), by HTTP's Basic or Bearer.
  - id: alice-jmap
    kind: jmap
    address: alice@example.com
    session_url: "https://mail.example.com/.well-known/jmap"

  # — Matrix — the device is created once by `uplink matrix pair` (§9.4).
  # No password in YAML.
  - id: alice-matrix
    kind: matrix
    homeserver: "https://matrix.example.org"
    mxid: "@alice:example.org"
```

An empty or missing file is refused with a pointer to `uplink init`;
buckets without a name or a query, of a name another has, or whose query
is not of the language are refused too. Unknown fields are refused, so a
field that went away (`data.root`,
`master_key_file`, `token_file`, `state_dir`, in configurations written
before these were derived) is reported rather than ignored.

### 3.3 Hot-reload semantics

| Section change | Action |
|---|---|
| `server.tls.cert` / `key` | Reload PEMs, swap `Arc<rustls::ServerConfig>`. No connection drops. Adding or removing `tls` needs a restart. |
| `server.basic_auth` | Update in-memory hash. New requests authenticate against new hash. |
| `accounts[].oauth.*` | Re-issue refresh token if file changed; restart that account's sync task. |
| `accounts[]` add/remove | Spawn / shut down sync task. Push `AccountListChanged` to clients. |

Account changes currently require a restart (logged and ignored, like
`server.bind`); applying them live is deferred to M10. A sync task
re-reads its token file on every refresh, so re-running `uplink oauth
init` needs no restart.
| `server.bind` | **Restart required.** Logged as warning, ignored at runtime. |
| `home` | The buckets are swapped; the notice `HomeChanged` (§5.5) has clients list home again. |

### 3.4 Why not env vars / multiple files

- The user is the operator. One file is one place to look.
- Hot-reload of a directory tree is more complex than watching one file.
- Env vars don't survive `systemctl reload` cleanly.
- A single file is easy to btrfs-snapshot alongside data.

---

## 4. Storage & schema

### 4.1 On-disk layout

```
$DATA/                                # data_dir
├── master.key                        # made on first use (§13.4)
├── db/
│   ├── uplink.sqlite                 # main DB
│   ├── uplink.sqlite-wal
│   └── uplink.sqlite-shm
├── mail/
│   └── <sha256>/<sha256>.eml         # content-addressed; 2-char shard dir
├── matrix/
│   └── <account_id>/                 # matrix-rust-sdk stores, sealed session
└── accounts/
    └── <account_id>.token            # sealed OAuth refresh token
```

The `.eml` files are **sacred** (invariant 4). They are written
once, read many times, never mutated. If the SQLite index becomes corrupt,
the entire derived state can be rebuilt by walking the directory and re-
indexing — this is a basic correctness lever, not a performance feature.

`mail/<sha256>/<sha256>.eml` — 2-char sharding gives ~256 dirs at the top
level. With `~1 M` messages that's ~4 K files per dir, fine for ext4/btrfs.

### 4.2 SQLite settings

- `journal_mode = WAL` — readers + one writer, snapshot-friendly.
- `synchronous = NORMAL` — fsync on checkpoint only. Acceptable durability
  given btrfs snapshots.
- `foreign_keys = ON`.
- `temp_store = MEMORY`.
- `cache_size = -64000` (64 MiB).
- One write connection, owned by the op dispatcher and used from the tokio
  blocking pool; read connections added as queries need them.
- All writes serialized through the op dispatcher; read connections run in
  the tokio blocking pool.

**Why rusqlite over sqlx**: no compile-time DB schema dependency (sqlx
verifies queries against a live DB at build time, which is brittle for an
embedded DB with migrations and FTS5). `rusqlite` has first-class FTS5
support and works directly from `spawn_blocking`.
For a single-writer embedded DB, sqlx's pool semantics buy nothing.

**Migrations**: `refinery` crate. Forward-only, embedded SQL files in
`crates/uplink-storage/migrations/V0001__init.sql`, etc. The CLI command
`uplink migrate` runs them; the server runs them automatically at startup
and refuses to start if the migration set has been edited (checksum
mismatch).

### 4.3 Schema (initial migration sketch)

```sql
-- ── mail (migrations/V0002__mail.sql) ────────────────────────────────
-- An account, as far as sync is concerned. Created by its first
-- observation; configuration (address, credentials) stays in the YAML.
CREATE TABLE account (
  id          TEXT PRIMARY KEY,
  kind        TEXT NOT NULL,       -- 'gmail' | 'm365' | 'matrix'
  sync_state  TEXT,                -- per-kind JSON; NULL until the first
                                   -- sync has begun
  synced      INTEGER NOT NULL     -- (V0013) the first listing is done, so
              DEFAULT 0,           -- the mailbox is mirrored
  listing_total INTEGER            -- (V0015) while it is not: the messages
                                   -- the mailbox held as it began (Gmail's
                                   -- profile, Spam and Trash included)
  push_error  TEXT,                -- (V0018) why pushes are failing, while
  push_failing_since INTEGER       -- they are (§6.6); unix ms
);

-- A logical message, identified by its Message-ID across accounts.
CREATE TABLE message (
  id                INTEGER PRIMARY KEY,
  message_id        TEXT NOT NULL UNIQUE,   -- RFC 5322, or synthetic (§4.4)
  preferred_variant TEXT NOT NULL,          -- sha256 of the variant shown
  -- Of the preferred variant, for listing and for the merge check:
  subject           TEXT,
  from_addr         TEXT,
  from_name         TEXT,
  to_addrs          TEXT NOT NULL,          -- JSON array of addresses
  cc_addrs          TEXT NOT NULL,
  date_sent         INTEGER,                -- Date:, unix seconds
  senders           TEXT NOT NULL,          -- JSON array: addresses a variant
                                            -- may name as its sender (§4.4)
  snippet           TEXT NOT NULL,          -- start of the body text
  has_attachments   INTEGER                 -- (V0014) files attached, not
                                            -- inline images; NULL until
                                            -- indexed, at startup, for
                                            -- messages stored before
);

-- One distinct copy of a message, stored at mail/<sha[0:2]>/<sha>.eml.
CREATE TABLE message_variant (
  sha256        TEXT PRIMARY KEY,
  message       INTEGER NOT NULL REFERENCES message(id),
  source        TEXT NOT NULL,     -- account id it was first fetched from
  size_bytes    INTEGER NOT NULL
);
CREATE INDEX message_variant_by_message ON message_variant(message);

-- A message as it exists in an account (a Gmail message, a Graph item).
-- An account may hold several copies of one logical message.
CREATE TABLE mail_entry (
  id           INTEGER PRIMARY KEY,
  account_id   TEXT NOT NULL REFERENCES account(id),
  external_id  TEXT NOT NULL,      -- Gmail message id / Graph message id
  message      INTEGER NOT NULL REFERENCES message(id),
  variant      TEXT NOT NULL REFERENCES message_variant(sha256),
  thread_id    TEXT,               -- Gmail threadId / Graph conversationId
  received_at  INTEGER NOT NULL,   -- unix ms (Gmail internalDate)
  UNIQUE (account_id, external_id)
);
CREATE INDEX mail_entry_by_message ON mail_entry(message);
CREATE INDEX mail_entry_by_thread ON mail_entry(account_id, thread_id, received_at, id); -- (V0016)

CREATE TABLE label (
  id           INTEGER PRIMARY KEY,
  account_id   TEXT NOT NULL REFERENCES account(id),
  external_id  TEXT NOT NULL,      -- Gmail label id ('INBOX', 'Label_1')
  name         TEXT NOT NULL,
  system       INTEGER NOT NULL,   -- provider-defined (0/1)
  -- (V0017) Kept by triggers (below): the conversations with the label,
  -- and those of them unread, as §4 counts them.
  conversations        INTEGER NOT NULL DEFAULT 0,
  unread_conversations INTEGER NOT NULL DEFAULT 0,
  UNIQUE (account_id, external_id)
);

-- Label membership of an entry, desired (set by ops) and observed (set by
-- sync); the account's reconciler pushes differences (§6.6). Read and
-- starred state are labels too (UNREAD, STARRED). Rows with both 0 are
-- deleted.
CREATE TABLE entry_label (
  entry      INTEGER NOT NULL REFERENCES mail_entry(id) ON DELETE CASCADE,
  label      INTEGER NOT NULL REFERENCES label(id) ON DELETE CASCADE,
  desired    INTEGER NOT NULL,
  observed   INTEGER NOT NULL,
  -- (V0018) The op that set `desired`, while it differs from `observed`:
  -- the outbox's item it belongs to (none for changes made before it was
  -- recorded); and the provider's refusal of it, after which it is not
  -- pushed again until an op changes it. Both go once it is reconciled.
  desired_by INTEGER REFERENCES tx_log(id) DEFERRABLE INITIALLY DEFERRED,
  refused    TEXT,
  PRIMARY KEY (entry, label),
  CHECK (desired OR observed)
);
CREATE INDEX entry_label_by_label ON entry_label(label, entry) WHERE desired;
CREATE INDEX entry_label_pending ON entry_label(entry) WHERE desired != observed;

-- (V0017) Label counts, kept as entries change rather than counted when
-- asked. A view says how an entry counts now (`entry_label_counted`: per
-- desired label, its conversation and whether it is unread, Trash and
-- Spam hiding it from other labels); `counted_label` holds how it counted
-- last, and `label_conversation(label, conversation, entries, unread)`
-- the sums per conversation. Triggers on changes to `entry_label`
-- (insert, delete, `desired`) and `mail_entry` (delete, `thread_id`)
-- queue the entry in `entry_recount`, whose trigger takes away how it
-- counted last and adds how it counts now, moving `label`'s counts where
-- a conversation's sum leaves or reaches 0. (Not a pair of before and
-- after triggers: an upsert that updates fires BEFORE INSERT ones too.)

-- Full-text index, one row per logical message (rowid = message.id).
CREATE VIRTUAL TABLE message_fts USING fts5(
  subject, from_addr, from_name, to_addrs, cc_addrs, body_text,  -- cc: V0031
  tokenize = 'unicode61 remove_diacritics 2',
  prefix = '1 2 3'                                               -- V0009
);

-- ── matrix (V0004; §9) ───────────────────────────────────────────────
-- Local ids, never reused. Rooms and events as synced; messages being
-- sent. Events are in sync order, which is also the timeline's.
CREATE TABLE matrix_room (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id   TEXT NOT NULL REFERENCES account(id),
  room_id      TEXT NOT NULL,        -- !opaque:server
  name         TEXT NOT NULL,        -- display name, as the SDK computes it
  encrypted    INTEGER NOT NULL,
  membership   TEXT NOT NULL,        -- 'joined' | 'invited' | 'left'
  read_up_to   INTEGER REFERENCES matrix_event(id), -- desired read marker
  read_pushed  INTEGER REFERENCES matrix_event(id), -- last receipt sent/seen
  desired      TEXT,                 -- 'joined' | 'left' asked for (§9.7)
  membership_by INTEGER REFERENCES tx_log(id), -- the op that asked; set iff desired (V0024)
  membership_error TEXT,             -- the homeserver's refusal of what is asked for
  UNIQUE (account_id, room_id)
);
CREATE TABLE matrix_event (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  room         INTEGER NOT NULL REFERENCES matrix_room(id),
  event_id     TEXT NOT NULL,
  sender       TEXT NOT NULL,
  sender_name  TEXT,
  ts           INTEGER NOT NULL,     -- origin_server_ts
  type         TEXT NOT NULL,
  msgtype      TEXT,                 -- of m.room.message
  body         TEXT,                 -- of m.room.message
  raw_json     TEXT NOT NULL,        -- the event, decrypted if it could be
  encryption   TEXT NOT NULL,        -- 'none' | 'decrypted' | 'undecryptable'
  pos          INTEGER NOT NULL,     -- where it is in the timeline (V0028, §9.2)
  UNIQUE (room, event_id)
);
CREATE TABLE matrix_gap (      -- events missing before an event (V0028)
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  room         INTEGER NOT NULL REFERENCES matrix_room(id),
  before_event INTEGER NOT NULL UNIQUE REFERENCES matrix_event(id)
);
CREATE TABLE matrix_send (     -- (V0023: was matrix_outbox)
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  room         INTEGER NOT NULL REFERENCES matrix_room(id),
  body         TEXT NOT NULL,
  queued_at    INTEGER NOT NULL,
  wanted       INTEGER NOT NULL,     -- desired, as mail_send's (§6.7)
  desired_by   INTEGER NOT NULL REFERENCES tx_log(id),
  state        TEXT NOT NULL,        -- 'queued' | 'sending' | 'sent' | 'refused'
                                     -- | 'taking_back' | 'taken_back' (V0037, §9.3)
  error        TEXT,                 -- why refused, or refused taking back (V0037)
  event_id     TEXT,                 -- set if sent, but of a member's change (V0029)
  synced       INTEGER NOT NULL      -- the event is synced: out of the outbox
);
-- Full-text search of Matrix messages is deferred.

-- ── calendar (M365 read-only; as built, V0012; §10) ──────────────────
-- One row per event *instance*, as a Graph calendar view lists them:
-- single events, and each occurrence or exception of a series.
CREATE TABLE calendar_event (
  id              INTEGER PRIMARY KEY,
  account_id      TEXT NOT NULL REFERENCES account(id),
  external_id     TEXT NOT NULL,            -- Graph id of the instance
  series          TEXT,                     -- seriesMasterId, if recurring
  exception       INTEGER NOT NULL,         -- an occurrence changed from its series
  subject         TEXT NOT NULL,
  location        TEXT NOT NULL,
  preview         TEXT NOT NULL,            -- bodyPreview
  start_ts        INTEGER NOT NULL,         -- ms since the epoch, UTC
  end_ts          INTEGER NOT NULL,
  all_day         INTEGER NOT NULL,         -- then start/end are UTC midnights
  cancelled       INTEGER NOT NULL,         -- isCancelled (V0046)
  show_as         TEXT NOT NULL,            -- showAs: free, tentative, busy, …
  response        TEXT NOT NULL,            -- responseStatus.response
  join_url        TEXT,                     -- its online meeting's (V0047)
  raw             TEXT NOT NULL,            -- the event's Graph JSON
  UNIQUE (account_id, external_id)
);
CREATE INDEX calendar_event_by_start ON calendar_event(start_ts);
-- The window listed and its delta link: {start, end, delta_link}.
CREATE TABLE calendar_sync (
  account_id      TEXT PRIMARY KEY REFERENCES account(id),
  state           TEXT NOT NULL
);

-- ── TODOs (local-only MVP; as built, V0005) ──────────────────────────
-- Ids are never reused (AUTOINCREMENT): log entries refer to them.
CREATE TABLE todo (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  title       TEXT NOT NULL,
  notes       TEXT NOT NULL,
  due         TEXT,                  -- a date, YYYY-MM-DD; no time of day
  done_at     INTEGER,               -- unix ms; NULL while open
  -- The message it was flagged from (§11.2). Message rows are never
  -- deleted (entries are), so the link holds.
  message     INTEGER REFERENCES message(id),
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);
CREATE INDEX todo_by_message ON todo(message) WHERE message IS NOT NULL;

-- ── drafts (local-only MVP) ──────────────────────────────────────────
-- Edited only through draft ops (§7); history lives in tx_log.
CREATE TABLE draft (
  id             TEXT PRIMARY KEY,          -- chosen by the creating client
  account_id     TEXT NOT NULL,             -- configured account sent from
  revision       INTEGER NOT NULL,          -- edits so far (§7.2)
  to_addrs       TEXT NOT NULL,             -- address lists as typed
  cc_addrs       TEXT NOT NULL,
  bcc_addrs      TEXT NOT NULL,
  subject        TEXT NOT NULL,
  body_html      TEXT NOT NULL,             -- the editor's HTML
  in_reply_to    INTEGER REFERENCES message(id),
  updated_at     INTEGER NOT NULL
);

-- ── sends (§6.7) ─────────────────────────────────────────────────────
-- Mail only; Matrix sends get their own table, as their payload and
-- reconciler differ. Ids are never reused: log entries refer to them.
-- (V0019: was mail_outbox; sends are kept once synced.)
CREATE TABLE mail_send (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id     TEXT NOT NULL,
  draft          TEXT NOT NULL,             -- JSON: the draft as sent
  message_id     TEXT NOT NULL UNIQUE,      -- generated when queued
  delay_ms       INTEGER NOT NULL,          -- the send delay queued with
  not_before     INTEGER NOT NULL,          -- unix ms
  wanted         INTEGER NOT NULL,          -- desired: 0 once cancelled while it can't be taken back
  desired_by     INTEGER NOT NULL REFERENCES tx_log(id), -- the op that set `wanted`
  state          TEXT NOT NULL,             -- 'queued' | 'sending' | 'unknown' | 'sent' | 'refused'
  attempted_at   INTEGER,                   -- unix ms: when the last attempt began
  provider_draft TEXT,                      -- Gmail, M365: the draft it is sent as (§8.2, §8.4)
  provider_thread TEXT,                     -- Gmail: that draft's thread
  provider_copy  TEXT,                      -- Gmail: its copy's message id, as Gmail told
  error          TEXT,                      -- set iff refused or unknown: why
  message        INTEGER REFERENCES message(id), -- set iff sent: the local copy
  synced         INTEGER NOT NULL           -- the provider's copy is observed, or can't be told
                                            -- (§8.2): out of the outbox
);

-- ── the log ─────────────────────────────────────────────────────────
-- Every change to uplink's state, in commit order (§6). User ops carry
-- their inverse; observations of external state (account_id set) have
-- none. Doubles as the event stream (§5.3).
CREATE TABLE tx_log (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  ts             INTEGER NOT NULL,          -- ms since epoch
  account_id     TEXT,                      -- NULL: user op; else the account observed
  description    TEXT NOT NULL,             -- human string, e.g. 'archived 12 messages'
  op_type        TEXT NOT NULL,             -- op / observation kind
  op_json        TEXT NOT NULL,             -- op / observation payload
  inverse_type   TEXT,                      -- inverse op kind
  inverse_json   TEXT,                      -- inverse op payload
  events_json    TEXT NOT NULL,             -- JSON array of the ServerEvents produced
  undone_by      INTEGER REFERENCES tx_log(id), -- set once this entry is undone
  undoes         INTEGER REFERENCES tx_log(id), -- set if this entry is an undo
  CHECK ((account_id IS NULL) = (inverse_type IS NOT NULL)),
  CHECK ((inverse_type IS NULL) = (inverse_json IS NULL)),
  CHECK (account_id IS NULL OR (undone_by IS NULL AND undoes IS NULL))
);
CREATE INDEX tx_log_by_ts ON tx_log(ts DESC);
```

### 4.4 Schema design notes

- **Message identity is the Message-ID, globally.** The same email in
  Gmail and M365 is one logical message (for a unified inbox) with
  per-account state in `mail_entry`. Every distinct copy is a
  `message_variant`; re-fetching identical bytes is idempotent (same
  sha256). Gmail rejects mail without a valid Message-ID, so a missing one
  is rare, but other paths can deliver it.
- **Merging is checked, not assumed.** A new variant joins an existing
  message only if it agrees on `Date:` (exact) and the sender address —
  where a mailing list rewrote `From:` for DMARC (`List-Id` present), on
  the original sender (`X-Original-From` / `Reply-To`). A variant that
  fails the check is a Message-ID *collision* (spam, broken clients): it
  becomes its own logical message under a synthetic Message-ID
  `<sha256@variant.uplink.invalid>` and the collision is logged. Variants
  without a Message-ID get the same synthetic form, so they never merge
  with anything. The preferred variant is the first one stored, so a
  later colliding copy cannot replace what is displayed.
- **Entries.** A `mail_entry` is a message as it exists in one account
  (a Gmail message, a Graph item), keyed by the provider's id; an account
  may hold several copies of one logical message. Labels attach to
  entries, since providers label their copies independently.
- **Accounts are recorded by their first observation** (`account.kind`,
  `sync_state`), so that every change to the database is a log entry;
  their configuration (address, credentials) stays in the YAML.
- **Variants are addressed by hash**: `mail/<sha256[0:2]>/<sha256>.eml`,
  so no path is stored. Message-IDs are stored without angle brackets.
- **No threads table.** `thread_id` is per account in `mail_entry`. Threading
  is whatever the provider tells us (Gmail `threadId`, M365 `conversationId`):
  Gmail's own grouping, subject-based included, and its cap of 100
  messages a thread. We don't synthesize threads when the provider
  doesn't give them; an entry without one is a conversation by itself.
- **Lists are of conversations**, as Gmail's are (in its Conversation
  view, the only one uplink has), and every list is a query (§8.7), as
  in notmuch: the Inbox is `in:inbox`. A conversation is the entries of
  one account's thread in the list's *scope* (in a query that names
  Trash or Spam, the entries with that label; with `in:anywhere`, all;
  in any other, those in neither). A list holds the conversations with
  an entry that matches, ordered by their newest entry in scope, which need
  not be on it itself: a reply the user sent brings a conversation in the
  Inbox to the top. Label counts are of conversations too.
- **Flags are labels.** Gmail already models read and starred as system
  labels (`UNREAD`, `STARRED`), so there is one desired/observed
  mechanism, not two. M365's `isRead` and flag status will map onto the
  same labels.
- **Soft delete is label removal, not `message` deletion.** The
  raw `.eml` stays on disk forever (invariant 4). An entry the provider
  deleted is removed with its labels, but its message and variants stay.
  Hard delete is deferred — see ROADMAP.
- **`tx_log` carries forward AND inverse.** Recorded inside the same SQLite
  transaction as the apply. The inverse is the inverse *edit* (§6.2), not a
  snapshot of pre-state.
- **`undone_by` / `undoes`** form pairs: when entry A is undone, a new entry
  B is appended with `B.undoes = A.id`, and a separate update sets
  `A.undone_by = B.id`. Both updates happen inside one transaction. Redo
  of an undo is a normal new op with `undoes = B.id`.

### 4.5 Body-text extraction for FTS

The fetch path writes the `.eml`, then parses it with `mail-parser`
(stalwartlabs) — outside the writer lock, before recording the
observation — taking the first `text/plain` part, or else the first
`text/html` part converted by mail-parser's own HTML-to-text. The
observation inserts the `message_fts` row (headers + the first 64 KB of
body text) when it creates the logical message; the preferred variant
never changes, so neither does the row. Parsing takes milliseconds and
fetching dominates, so no separate indexing queue is needed. The parsed
fields are not part of the recorded observation: they are derived from
the `.eml`.

---

## 5. Wire protocol (WebSocket)

### 5.1 Transport

- One WebSocket per browser tab. Path: `/ws`. Subprotocol: `uplink.v1`.
- Authenticated by the same basic-auth as `/`. The browser sends the
  `Authorization` header on the WebSocket upgrade because the page itself
  was loaded under basic auth. (Sessions/cookies would be more robust over
  the long run; deferred — see §15.)
- Messages are JSON text frames (UTF-8). No binary frames in MVP. Binary
  payloads (attachments, images) go through HTTP under `/api/...`.
- App-level heartbeat: client sends `{"type":"ping","seq":N}` every 30 s;
  server responds `{"type":"pong","seq":N}`. Server closes after 60 s of
  silence.
- Versioned by `Hello`'s `version`: a hash (FNV-1a, 64 bits) of the wire
  types' TypeScript declarations (`uplink_protocol::typescript`), which
  `web/src/generated/protocol.ts` ends with as `PROTOCOL_VERSION` (the
  `protocol-ts` check keeps it current). Any change to the types changes
  it, with nothing to remember to bump. A server speaking another closes
  the connection with code 4002; the client, a page of another build,
  reloads itself rather than reconnecting (it would be refused again,
  each time, as a tab left open across an upgrade was, every second).

### 5.2 Message shape

All messages use a tagged enum discriminator. In Rust:

```rust
// crates/uplink-protocol/src/lib.rs

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsClientMsg {
    Ping { seq: u64 },
    /// First message after connection: the protocol version the client
    /// speaks (§5.1), and the last entry it saw, if it wants to resume.
    Hello { version: String, since: Option<TxLogId> },
    /// Request/response: the matching reply carries the same request_id.
    Request { request_id: u64, body: ClientRequest },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsServerMsg {
    Pong { seq: u64 },
    /// Reply to Hello (see §5.3).
    Welcome { cursor: TxLogId, resumed: bool },
    Response { request_id: u64, body: ServerResponse },
    RequestFailed { request_id: u64, error: ServerError },
    /// A log entry, as committed; replayed or live.
    Entry { entry: LogEntry },
}

pub struct LogEntry {
    pub id: TxLogId,
    pub ts: i64,
    pub source: Source,          // User | Account { id }
    pub description: String,
    pub op_type: String,
    pub undoes: Option<TxLogId>, // set on undo entries
    pub events: Vec<ServerEvent>,
}
```

Payloads nest under `body` rather than being `#[serde(flatten)]`ed: they
are tagged enums themselves, and their `type` tag would collide with the
envelope's.

`ClientRequest`, `ServerEvent`, `ServerResponse` are
themselves tagged enums; per-domain variants live in domain submodules
(`protocol::mail::*`, `protocol::matrix::*`, …) and are re-exported.

### 5.3 Replay

Clients follow the log: the cursor is the id of the last entry received,
and it means the same thing across server restarts. On
`Hello { since: Some(id) }`, the server replies `Welcome { resumed: true }`
followed by every entry after `id` read from SQLite, then live ones. It
replies `Welcome { resumed: false, cursor }` instead when the client says
`since: None`, is more than 4096 entries behind, or names an id the log
never had; the client then discards its state, reloads, and adopts
`cursor` as its position.

The session subscribes to the hub's live fan-out *before* reading the log,
and skips live entries it already replayed, so nothing committed in
between is lost. A session that falls further behind the live fan-out
than it buffers is closed; the client reconnects and resumes.

### 5.4 Event types (MVP)

`ServerEvent` variants by domain:

- **Mail**: `EntriesAdded`, `EntriesRemoved`, `LabelsChanged` (desired
  and observed membership per entry and label), `LabelListChanged`,
  `ImageRuleChanged { rule, always }` (§8.9).
  Events carry ids, not content, since they are stored in the log;
  clients query what they display. As the log is replayed, a change to
  an event's shape migrates the recorded ones too (V0022 did, for
  `OutboxChanged`).
- **Matrix**: `RoomsChanged { rooms }` (appeared, or name, membership,
  unread count or read marker changed), `TimelineAdded { room, events }`
  (appended to the timeline), `TimelineChanged { room, events }` (what
  those events show changed: they were edited, reacted to or redacted,
  or what they reply to was; §9.2), `MatrixSendsChanged { rooms }` (their
  sends were added, changed state or went).
- **Calendar**: `CalendarEventAdded`, `CalendarEventRemoved`,
  `CalendarEventChanged`.
- **To-dos**: `TodosChanged { todos }` (added, edited or deleted; the
  client lists them again, as it does drafts).
- **Draft**: `DraftEdited { draft, revision, splice }` (§7.2), and
  `DraftsChanged { drafts }` when drafts are created or deleted.
- **Outbox**: `OutboxChanged { accounts }` — something not yet reconciled
  in those accounts was added, changed state or went (§6.6); clients
  showing the outbox ask for it again.
- **Config**: `ConfigChanged { section }`.

`ClientRequest` variants (MVP):

- `ListAccounts` — mail accounts with their labels and counts, and,
  until an account's first listing is done, how far it has got: the
  messages stored of `listing_total`, which the nav shows as a
  percentage; its tooltip estimates the time left, from the pace the
  count rose at over the last ten minutes the page saw.
- `ListEntries { query, after, limit }` — a page of the conversations a
  query lists (§4, §8.7), in every account, newest first: a label's is
  `account:… label:…`, every account's Inbox `in:inbox`, an account's All
  Mail (§8.1) `account:…`, and a search the user's query. Each has its
  newest entry in full, the others' ids, messages and labels (enough to
  act on them and follow label changes), its senders, and whether any
  has attachments; a page ends at its last conversation's newest entry.
  The response says the list's scope, as labels an entry has one of or
  none of, so the client keeps it as labels change; how a search's
  matches change, only the server knows, so the client reloads it. A
  query that does not parse, or names a label or account there is none
  of, is `Invalid`, with why.
- `CountConversations { query }` — how many conversations a query
  lists, as a search does, loaded or not: the toolbar's offer to select
  every match.
- `ListConversation { entry }` — the entries of the entry's conversation,
  in Trash and Spam too (the client keeps the list's scope), oldest
  first: the conversation view's.
- `ListMessageEntries { message }` — a message's entries, in every
  account: a to-do's message.
- `LoadMessage { message }` — headers, the text body, and the sanitized
  HTML body if there is one, its remote images' hosts (with how many
  of each), how many likely tracking pixels it has, and the user's rules
  that show its images unasked (§8.9).
- `ListRooms` — Matrix accounts and their rooms with unread counts;
  `ListTimeline { room, page, limit }` — the latest events, or those
  before or after one, oldest first, each with what is folded into it
  (§9.2), and, of a page that reaches the end (the latest, or one after
  an event that is not full), the room's sends not yet synced back,
  which follow it: with the events, so that a send synced back shows as
  one or the other, never both or neither; `ListRoomEvents { room, events }` — those of the events the
  timeline lists, as it lists them (for `TimelineChanged`);
  `ListEdits { room, event }` — a message's edits, oldest first, for its
  edit history; `ListThread { room, root }` — a thread (§9.2);
  `ListPinned { room }` — of the room's pinned events, those stored and
  listed, in the order pinned (§9.5);
  `PreviewUrl { room, event, url }` — a link's preview (§9.8);
  `ResolveRoomAlias { account, alias }` — the room an address is of
  (§9.7);
  `SearchRoom { room, query, before, limit }` — the room's messages
  (of the main timeline and its threads, not deleted) whose text, or an
  edit's, has every word of the query (ignoring the case of ASCII
  letters, as SQLite's `LIKE`), newest first, a page at a time, and
  whether there are more. Messages are stored decrypted, so encrypted
  rooms are searched too.
- `Undo { tx_log_id }`; `ListHistory { before, limit }` — the user's
  ops, newest first, each with the entry that undid it, if any (§6.3).
- `ListDrafts` — all drafts. `ListOutbox` — per account, its push
  error, if failing, and what is not yet reconciled (§6.6), by the kind
  of account: of mail, label changes grouped by the op that made them
  (its description, the entries left, how many refused, with their
  reasons) and sends; of Matrix, sends, rooms being joined or left (with
  the op that asked, and the refusal), and the rooms whose read receipt
  is still to send; each with its state. The server lists both kinds in
  one read, so as of one log position.
- One request per op kind the client can dispatch, answered with the
  tx_log id: `ActOnConversations { conversations, action }` does an
  action to conversations (§12.8), as one `SetLabels` op: the
  conversations as a list shows them (each its entries in the list's
  scope), or every one a query lists, as a search does, when the op is
  done; `ModifyLabels { entries, add, remove }` changes labels of
  entries (mark unread from a message on); `CreateDraft`, `EditDraft { draft, revision,
  splice }`, `DeleteDraft`, `SendDraft`, `CancelSend { item }`,
  `KeepSent { item }` (§6.7, §7); `KeepObserved { op }` — a refused
  change taken back to what the provider has (§6.6); `AlwaysShowImages
  { rule, always }` (§8.9);
  `SendMatrixMessage { room, body }`, `CancelMatrixSend { item }`,
  `KeepMatrixSent { item }`,
  `MarkRoomRead { room, event }`, `ChangeMembership { room, change }`,
  `OrderSpaces { changes }` (§9).

Query responses carry `as_of`, the last log entry they reflect; `Undone`
and `Dispatched` reflect the log up to their own entry. The session sends
every entry up to that position before the response, and only later ones
after it, so a client replaces its state with a response and applies every
entry that follows on top, without comparing ids. (Requests are handled
one at a time per session, and the hub publishes in commit order, so the
session only ever waits for entries already committed.)

---

### 5.5 Notices

What is only of the moment — who is typing, that members sent read
receipts, that the account's devices or keys changed, that the home
view's buckets were configured anew — is not logged:
the server pushes it to the sessions connected as `Notice { notice }`,
live only, never replayed. A session that falls behind the notices skips
those it missed. A client that reconnects forgets what notices told it
(no one is typing) and loads again what they invalidate. Requests of the
moment (`SetTyping`) are not ops either: they answer `Done` and log
nothing, and queries of state not logged (`ListReceipts`, `ListMembers`) are not
positioned in the log, so a client loads again after one in flight when
told of a change meanwhile.

State derived from the log, which every session needs as it is, is
pushed rather than asked for: the badges (§12.14), as `Badges { badges }`
right after `Welcome`, and again whenever they change. The server
derives them once, for every session (`uplink_realtime::Hub` keeps them),
before it serves any.

### 5.6 Requests: local now, remote later

A session handles its requests one at a time, in the order sent: each
request's ops commit before the next is looked at, so ops sent without
waiting for each other's answers (starring and unstarring, typing and
stopping) take effect in that order. So that no request holds up those
after it (nor the pings: a session waiting on one is not reading), what a
request does at once is local only — the database, the clients' own
stores — and never waits on anything remote. Ops are desired state
(§6): they record what is wanted, and tasks of their own carry it out
remotely, in order.

A request that needs a homeserver's answer (`PreviewUrl`, `FillGap`,
`ListMembers`, `GetMatrixSecurity` and the requests answered with the
security state, `CreateMatrixRoom`, `StartDirectChat`, `JoinMatrixRoom`,
`ResolveRoomAlias`, `ImportRoomKeys`) does its local part (checks, reads) in turn, then
hands what waits on the homeserver to a task of its own, and the session
goes on; it answers when the task is done, as `Answer::Later`. Responses
thus come in any order, as the client already takes them, by
`request_id`; one to a request the client no longer cares for, it drops.
The task runs to its end even if the session ends first, so that what it
stores (a gap's events) is not lost: it reaches the client through the
log, and a client that lost the connection asks again what it still
wants (§12.3). What waits remotely and must keep its order is desired
state instead (`SetTyping`, §9.5). A request answered later is still
positioned in the log as one answered at once (§5.4).

## 6. Transaction log & undo

### 6.1 The dispatcher

```rust
// crates/uplink-tx-log/src/lib.rs

pub trait Op: Serialize + DeserializeOwned + Send + 'static {
    /// Discriminator recorded in tx_log, e.g. "mail.archive".
    const KIND: &'static str;

    /// Apply within `tx`, computing the inverse from pre-state. On error,
    /// the transaction rolls back.
    fn apply(self, tx: &Transaction) -> Result<Applied>;
}

pub struct Applied {
    pub description: String,       // what it did, e.g. "Archived 3 messages"
    pub inverse: Inverse,          // Inverse::of(&some_op)
    pub events: Vec<ServerEvent>,  // broadcast after commit
}

impl Dispatcher {
    pub async fn dispatch<O: Op>(&self, op: O) -> Result<TxLogId>;
    pub async fn undo(&self, id: TxLogId) -> Result<TxLogId, UndoError>;
}
```

The dispatcher owns the single write connection. `dispatch` begins a
transaction, applies the op, inserts the tx_log row (forward and inverse
kind + JSON, and the events produced), commits, then publishes the entry
— while still holding the writer, so publication order equals commit
order. `observe(account, observation)` does the same for observations of
external state, which record no inverse; `undo` refuses them.

Key properties:

- **Op application and tx_log INSERT are in the same SQLite transaction.**
  If the op fails, no log entry; if the log INSERT fails, the op rolls back.
  Atomic.
- **Broadcast happens after commit.** Subscribers see only committed state.
- **The inverse is computed inside `apply`** from what the op actually
  changed. For mass operations this is one inverse describing all the
  per-row changes.
- **Op author writes the inverse.** No automatic introspection. Op-specific
  knowledge of which fields changed is encoded in `apply`.
- **The description comes from `apply`**, so it says what actually
  happened, in the user's terms (label names rather than ids, the count of
  messages that actually changed).
- **An op knows its own entry's id** (`this_entry(tx)`: ids are never
  reused and the dispatcher is the only writer, so it is the next one,
  which `record` asserts), so that state can name the op that set it, as
  pending changes do (§6.6).

### 6.2 Inverses

An inverse is itself an op: `Inverse::of(&op)` records its kind and JSON.
Op kinds are registered in a `Registry` by the crates that define them
(tx-log sits below the domain crates, so it cannot name their types).
Dispatch rejects an op whose inverse kind is unregistered, so every
recorded entry can be undone.

**An inverse performs the inverse edit only** — it never restores a
snapshot. Undoing "archive these 50" re-adds INBOX to exactly the messages
the archive removed it from; changes made to them since are untouched.
Edits that replace a value (set a field from A to B) record the value
they replaced, and their inverse expects to find B: if the value changed
again in the meantime, undo **refuses** with a conflict rather than
overwriting. This is the same conflict as in desired-state sync (§6.6);
a future UI may let the user pick which version to keep.

Ops change desired state, never provider state directly (§6.6), so every
inverse is itself just a desired-state change. Whether the provider can
still be brought in line is the reconciler's to find out, not the
inverse's to decide: un-sending a mail that has gone out is applied,
and shown in the outbox as irreconcilable until resolved (§6.6, §6.7);
a Matrix message is taken back as nearly as Matrix allows, by redacting
it (§9.3).

### 6.3 Undo

The `Undo { tx_log_id }` request:

1. Look up tx_log entry by id.
2. If `entry.undone_by IS NOT NULL`, error: already undone.
3. Construct an `Op` from `entry.inverse_json` (an `inverse_type` lookup
   in the registry).
4. Apply and record it like any op, with `undoes = entry_id`, and set
   `tx_log[entry_id].undone_by = new_id` — all in one transaction.

Redo of an undo is just "undo the undo entry". This means the user can
toggle back and forth.

The history view (`/history`, M10) lists the user's ops, newest first,
from `ListHistory` (a partial index on `tx_log(id) WHERE account_id IS
NULL` keeps that quick however many observations syncing records), with
Undo on each not yet undone, and Redo on undo entries. The client loads
it once per connection and then applies the entries that follow: a
user's op goes on top, an undo marks the entry it undid. What views did
by being shown (§12.15) is listed dimmed.

### 6.4 Mass operations

A mass operation (e.g. "archive 50 selected messages") is a *single* Op,
not 50 ops. Its `apply` mutates 50 rows, computes a single inverse covering
the rows it changed, and produces one tx_log entry. The UI shows it as
"Archived 50 messages" with one Undo button.

Without this property, the undo log would be flooded with per-row noise
and undo would have to replay 50 entries to roll back one user gesture.

### 6.5 Which operations go through the dispatcher

**Every change to persistent state** is a log entry: user-initiated ops,
and observations of external state. User op examples:

- Marking message read/unread, archiving, labeling, deleting.
- Editing a draft (§7), sending mail, sending a Matrix message (§6.7).
- Creating, editing, completing a TODO.
- Flagging a message for follow-up (creates a linked TODO).

Observations (`Dispatcher::observe`, recorded with the observing
account): new mail arriving, Matrix events streaming in, calendar deltas,
and reconciler bookkeeping (§6.6). They have no inverse and cannot be
undone. Bulk ingest (e.g. the first sync of a mailbox) records one entry
per batch, not per message, and event payloads stay small (ids and
changed fields, not bodies), since they are stored.

Not log entries: read-only queries (search, load message body, load room
timeline).

### 6.6 Desired state, reconciliation and the outbox

Ops record what the user wants; they do not call providers. Everything
that mirrors a provider is stored twice: *desired* (set by ops) and
*observed* (set by sync, and by the reconciler as it learns what the
provider made of a push):

- a mail entry's labels: its labels, folder, read and starred state;
- whether a mail or a Matrix message is sent (§6.7);
- a Matrix room's membership (§9.7) and read receipt, and a space's
  order (§9.5).

Each account runs a reconciler (its sync task) that, after every commit
touching its state, pushes the differences — Gmail `messages.batchModify`
and draft `send`; Graph `PATCH`, `move` and draft `send`; Matrix sends,
joins, leaves, receipts and space orders. The UI shows desired state
immediately, so actions feel instant and survive a provider outage.

A difference is *pending* until the provider is observed to match. Each
records the op that made it (`desired_by`), so that it can be told in
the user's terms. A push ends one of four ways:

- **done**: the provider matches; the difference is gone;
- **not done, for a passing reason** (connection refused, 5xx, 429, token
  refresh): still pending, retried with the task's backoff. The account
  records, as an observation, when its pushes start failing (with the
  error) and when they recover, not each retry;
- **refused** by the provider (4xx other than for rate): the difference
  is marked refused, with the provider's reason, and not retried; the
  rest are pushed regardless (a refused batch is split until the
  differences it refused are found);
- **outcome unknown** (a timeout, the process stopped mid-request):
  still pending. A push states what should be, not a step to take, so
  it is pushed again: harmlessly for labels, receipts and membership,
  and, for sends, only once the provider has been observed not to have
  the message (§6.7).

A difference that can't be reconciled is *irreconcilable*: one refused,
or one asking for what no push can do, such as un-sending a message that
has gone out. It stays until the user resolves it by changing desired
state, usually to the observed ("keep Gmail's", "keep it sent"), with an
op that is logged and undoable like any other; or otherwise (a refused
send's draft fixed and sent again). Undo is therefore never refused for
the provider's sake: what it asks of the provider is reconciled or
shown, like everything else. (Undo refuses only conflicts in uplink's
own state, §6.2.)

When sync observes a remote change:

- if there is no pending local change for that field, desired follows
  observed (the remote change wins);
- if there is, the local change wins and is pushed. Surfacing such
  conflicts to the user is deferred, together with undo conflicts (§6.2);
- an entry the provider deleted takes its pending changes with it: there
  is nothing left for them to apply to.

**The outbox** (`/outbox`) shows everything not yet reconciled: per
account, the ops whose desired state the provider does not reflect yet,
newest first, each with what is left of it ("Archived 214
conversations: 180 to go") and its state: waiting (a send's delay),
being pushed, retrying (with the account's error, and since when), or
needing a decision (refused or irreconcilable: the reason, and the
actions that resolve it). Each has its Undo. Changes pending from before
ops were recorded with them (V0018) are listed together, as made
earlier. Sends are listed until the
provider's copy is synced (§8.6), so a sent mail stays until it is in
Sent. The nav shows the outbox's count, and marks it while an account's
pushes are failing or something needs a decision. An empty outbox means
the providers, as last observed, match what the user has done.

### 6.7 Sends: desired state fulfilled once

Sending mail or a Matrix message is desired state like any other: that
the message be sent from the account. It differs in how it is fulfilled:
once, by the provider accepting the message, after which nothing undoes
it.

`mail.send` (`SendDraft`) turns the draft into a send (`mail_send`),
wanted, with a Message-ID chosen there and then and `not_before` the
send delay (`send_delay_secs`, default 10) away; the draft leaves Drafts.
Its inverse, `mail.cancel_send` (also an op of its own, `CancelSend`),
makes the send unwanted, and is never refused:

- before the reconciler has claimed the send, it is fulfilled at once:
  the send goes, and the draft is back. The send delay is therefore a
  reliable undo-send window;
- once claimed (`sending`, the push in flight; `unknown`) or sent, it is
  a pending difference that only an outcome of not sent or refused
  fulfils (then the draft is back). If the message went out, it is irreconcilable ("sent before the
  undo"), resolved by keeping it sent (`mail.keep_sent`), which makes the
  send wanted again; the draft stays gone.

Sends are kept once sent, rather than deleted when synced, so undoing an
old send is shown the same way.

The reconciler claims a due send before handing it to the provider, so
that a cancel knows whether it can still be fulfilled locally. Then:

- accepted: `sent`, with the message as sent (§8.6);
- refused (4xx), or unbuildable (bad addresses): `refused`, with the
  reason; it needs a decision, and cancelling (the draft back for
  fixing) is fulfilled at once, as the provider has nothing;
- not sent for a passing reason: back to `queued`, retried with backoff;
- outcome unknown (a timeout; the process stopped while `sending`):
  `unknown`, until the provider is observed:
  - Gmail files the sent copy in accepting a send, but under a
    Message-ID of its own (§8.2), so sync can't tell by ours. It sends
    a draft created first, by its id, so sending it again is safe
    regardless: once a sync has run long enough (5 minutes) after the
    attempt that Gmail is done with the request, the send is queued
    again, and a draft already sent is gone, so it is not sent twice.
    Open: one no longer wanted is taken back then, its draft back,
    though it may have gone out; asking Gmail about its draft (gone
    means sent) would tell;
  - Graph the same way, its copy filed in Sent Items. As Graph sends a
    draft created first, by its id (§8.4), sending it again is safe
    regardless: a draft already sent is not sent twice.
  - JMAP the same way: the message is imported as a draft first, and
    submitted by its id, its copy moved to Sent as the server accepts
    it (§8.13); before a draft is submitted again, its submissions are
    asked for, so it is not submitted twice.
  - SMTP (IMAP and POP3 accounts) not so: the copy in Sent is filed by
    uplink once the server accepted the message, so its absence shows
    nothing, and the send stays unknown until a copy turns up or the
    user decides (§8.11). Providers say which they are (`FILES_SENT`).

SMTP and Gmail's `messages.send` take no idempotency key (RFC 1047
describes the duplicate a lost acknowledgement causes); observing the
outcome, by the Message-ID or the draft, is what makes a retry safe.
Gmail doesn't deduplicate by Message-ID either, as it replaces it.

Matrix messages (§9.3) differ in three ways. They have no send delay,
since chat expects immediacy: a cancel is fulfilled at once only until
the task claims the message. Sent, a cancel is fulfilled as nearly as
Matrix allows: the message is redacted. And the homeserver deduplicates a transaction
id, so a message whose outcome is unknown is simply sent again. Showing
queued items in Sent / the room timeline as a local echo is deferred;
the outbox stands in for it.

---

## 7. Drafts

### 7.1 What we actually need

Single user, a few tabs (or laptop+phone, eventually). Tab A is typing in
the compose window, tab B has the same draft open. We want:

- Tab B sees tab A's edits within ~500 ms.
- If both edit at once, neither's edit is silently dropped.
- If the network drops, tab A's text survives locally and flushes on
  reconnect.

We do not need operational transforms, CRDTs, or presence.

### 7.2 Draft edits are ops

Draft changes are ordinary tx_log ops, so undo, history and broadcast
need nothing draft-specific:

```rust
/// Creates a draft (revision 0), or restores a deleted one.
struct PutDraft { id: DraftId, account: String, revision: i64, content: DraftContent, in_reply_to: Option<MessageId> }
struct DeleteDraft { id: DraftId }
/// Replace `delete` with `insert` at `offset` of one text field, based on
/// the draft at `revision`.
struct EditDraft { id: DraftId, revision: Option<i64>, splice: Splice }
struct Splice { field: DraftField, offset: u32, delete: String, insert: String }
```

Every field is text: To, Cc and Bcc are address lists as typed, parsed
when sent, so they are edited like the subject and body. Offsets count
UTF-16 code units, as JavaScript strings do; a splice inside a surrogate
pair is refused.

An edit names the revision it was computed against, and is refused if
the draft has moved on: checking only that `delete` is at `offset` would
not catch an insertion landing in the wrong place after an edit from
another tab. Each edit increments the revision. The inverse swaps
`delete` and `insert` and carries no revision, since undo applies it to
whatever revision the draft has reached; it checks only that the text it
removes is where it expects (§6.2), so edits can be undone one at a time,
in any order that keeps their text in place. Each entry stores only the
changed span, never a copy of the draft.

The client (`web/src/compose/session.ts`) keeps the text as typed and the
server's draft it is based on. Typing pauses for 500 ms, then one edit
goes out for the first field that differs (common prefix/suffix, never
splitting a surrogate pair), one edit in flight at a time. When the
server's draft changes, the client recognizes its own edit (the next
revision, with exactly its change) and otherwise rebases: its own
changes, as one span against the old base, are moved past the remote
change and applied to the new text. Where the two overlap, the local
change gives up deleting what the remote one changed, so neither side's
insertion is lost. An edit in flight meanwhile is refused as stale and
recomputed. The editor's undo keeps operating on keystrokes; tx_log undo
steps back one saved edit at a time.

Creating and editing drafts are safe to resend after a reconnect, since
neither can apply twice (the id exists; the revision moved on), so the
connection resends them like queries.

### 7.3 History

Runs of consecutive edits to the same draft are shown collapsed as one row
in the log UI ("edited draft 'Re: lunch'"), expandable. Because undo is
appended rather than destructive, the history of a draft is never lost —
in the spirit of vim's and emacs' persistent undo.

### 7.4 Attachments

A file is attached in two steps. The browser uploads it, `POST
/api/blobs` with the file as the body, into the blob store: `blobs/` in
the data root, content-addressed like the mail store (§4.1), and answered
with `{blob, size}` (the SHA-256). Then `AttachToDraft {draft, blob, name,
mime}` attaches it, as an op of its own:

```rust
/// With an `id`, reattaches the attachment as it was before it was
/// detached (the inverse of `DetachFromDraft`).
struct AttachToDraft { draft: DraftId, id: Option<DraftAttachmentId>, blob: BlobId, name: String, mime: String, size: i64 }
struct DetachFromDraft { draft: DraftId, attachment: DraftAttachmentId }
```

Uploading is over HTTP rather than the WebSocket because files are large
and binary; it changes nothing but the blob store, so it needs no log
entry, and a blob attached to nothing is harmless. Attachments
(`draft_attachment`) are part of the draft (`Draft.attachments`), but not
of its text: attaching and detaching leave the revision as it is, so they
never conflict with edits in flight, and they announce themselves with
`DraftsChanged`, upon which tabs reload the drafts. Discarding, sending
and undoing either take the attachments along (`PutDraft` and the
send's copy of the draft include them). Blobs are never deleted: log
entries refer to them, as they do to mail.

The server refuses names that are empty or contain control characters,
MIME types that are not `type/subtype`, unknown blobs, and attachments
totalling more than 25 MB, Gmail's limit (uploads are cut off there
too). The client sends the browser's MIME type for the file, or
`application/octet-stream` when it has none.

Forwarding takes the original's files along (not the images its body
shows, since the forward quotes its text): `POST
/api/messages/{message}/attachments/{index}/blob` copies each into the
blob store, server-side, and the client attaches it.

In the compose window, Attach opens the file picker; files can also be
dropped onto it. Each attachment shows as a chip, with × to remove it.
Send waits until uploads have finished.

### 7.5 IndexedDB write-through (deferred)

Unsaved text survives a dropped connection without it: the tab keeps it,
and edits are resent on reconnect. Only closing the tab while
disconnected loses it, which is offline use, a non-goal (VISION). If it
turns out to matter: write every debounced edit to IndexedDB before it is
sent, remove it once acknowledged, and re-send pending edits on startup.

---

## 8. Email pipeline (Gmail, M365, IMAP, POP3, JMAP)

### 8.1 Gmail — receive

All Gmail access goes through the Gmail REST API (`reqwest`, no SDK). Its
model — one message with many labels, `threadId`, a `historyId` change
feed — matches uplink's (desired/observed labels, §6.6) far better than
IMAP's per-folder copies, and a REST fake is practical to test against
(§15.10).

- **Initial sync**: `messages.list` (all mail, newest first) for ids, then
  `messages.get?format=raw` for content, stored as variants (§4.4), most
  recent first. Each batch of 50 is one log entry: a page of 500 takes
  about 6 minutes of quota, and its messages show, and survive an
  interruption, as they come. Record the profile's
  `historyId` *before* starting, so nothing that changes during the walk
  is missed. The listing resumes after an interruption: `sync_state`
  holds `{listing_from, before}`, the history id it began at and the
  last entry recorded before it. A resumed listing lists anew (page
  tokens do not last), fetches only messages it has not, and fetches
  labels again only of entries up to `before`, since the polls from
  `listing_from` cover changes to those it fetched. Then the state
  becomes `{history_id: listing_from}`, and the account `synced`.
  The listing goes in steps, each one page listed or one batch fetched,
  between which the account's task pushes label changes and sends due
  mail (§6.6, §6.7): a first sync takes days, and what the user does
  meanwhile reaches Gmail at once rather than after it. The account task
  asks for the next step without waiting for the poll interval. Where it
  has got (page token, ids listed, ids yet to fetch) is kept in memory
  only, and a failed step is retried from there.
- **Quota**: Gmail allows a user 6,000 quota units a minute (projects
  created since 1 May 2026, as a user's own will be; older ones keep
  15,000) and refuses with 403 `rateLimitExceeded` for the rest of the
  minute once they are spent. Since May 2026, `messages.get` costs 60
  with `format=raw` (20 with `minimal`), so a mailbox downloads at about
  100 messages a minute at most. Each account's requests are paced by a
  token bucket of 80 units a second, 400 at once: 80 messages a minute,
  so a mailbox of 50,000 takes about 10 hours to list.
- **Incremental**: poll `history.list?startHistoryId=…` every 5 s (2
  quota units per call against 6,000 units/minute per user, so even 1 s
  polling is affordable) —
  `messagesAdded`, `messagesDeleted`, `labelsAdded`, `labelsRemoved` map
  directly onto observations. Persist the last `historyId` in
  `account.sync_state`, in the same transaction as the observations. If
  Google answers 404 (history id too old), fall back to a full resync.
- **Push (deferred)**: the API's only push is `users.watch` → Cloud
  Pub/Sub. No public endpoint is needed: Pub/Sub's REST
  `subscriptions.pull` (without `returnImmediately`) waits a bounded time
  for a message, i.e. a plain-HTTPS long-poll. Costs: a topic and
  subscription in the user's GCP project with
  `gmail-api-push@system.gserviceaccount.com` allowed to publish, the
  `pubsub` OAuth scope, and renewing the watch (Google recommends daily;
  it lapses after 7 days). A notification only says "something changed";
  the reaction is the same `history.list` call. Polling stays as the
  baseline, as Google also recommends, since notifications can be delayed
  or dropped. No IMAP anywhere.
- **Threading**: Gmail's `threadId`.
- **Labels**: `labels.list`; each label becomes a `label` row.
- **Trash and Spam** are labels like any other (the listing includes
  them, `includeSpamTrash`), but, as in Gmail, an entry with either is
  shown only in Trash or Spam: other labels' lists and counts, and
  search, leave it out. Trashing (`#`) adds TRASH and leaves INBOX, so
  undo is just removing TRASH; reporting spam (`!`) adds SPAM, and `!` on
  spam removes it and adds INBOX, as Gmail's "Not spam" does. Both go to
  Gmail through `batchModify`, which takes TRASH and SPAM.
- **All Mail** is not a label: it is every entry of the account outside
  Trash and Spam (`/all/:account`, `ga`).

### 8.2 Gmail — send

- **Transport**: in two steps, as M365's (§8.4). First the RFC 5322
  message we built becomes a draft, `users.drafts.create`, POSTed as a
  multipart upload (`/upload/...?uploadType=multipart`: JSON metadata
  with the message's `threadId`, then the message as `message/rfc822`),
  which takes messages of up to 35 MB; a JSON body with the message
  inlined as base64url is limited to 5 MB, too little for attachments.
  The draft's id and thread are recorded on the send (`provider_draft`,
  `provider_thread`, by `mail.send_drafted`). This sends nothing, so
  whatever its outcome it counts as not sent; one whose answer was lost
  leaves a draft behind in Gmail's Drafts. Then `users.drafts.send`
  sends the draft by its id.
- **Gmail's copy**: Gmail sends mail under a Message-ID and a `Date:` of
  its own, whatever ours were (from `messages.send` and `drafts.send`
  alike; SMTP would keep them, but doesn't file the copy in Sent). So
  its copy in Sent is not found by our Message-ID, and it is no variant
  of the same logical message by §4.4's rules. Instead `drafts.send`
  answers with the copy's message id, recorded on the send
  (`provider_copy`), and once the copy is synced the message as sent
  joins its logical message (§8.6). A draft sent is a new message of
  the draft's thread, and the draft is gone: sending it again answers
  404 and sends nothing. So the copy of a send whose answer was lost
  is looked for in the draft's thread: the message in Sent there, from
  no earlier than the send was due (less a minute, for clock skew), and
  no other send's copy. With one, it is the copy; with none, the draft
  was deleted at Gmail, which refuses the send (the user decides); with
  several, which is the copy can't be told, and the send is left
  synced, its message as sent apart.
- **Why API not SMTP**: avoids "Sent" folder synchronization mismatch
  (Gmail SMTP doesn't append to Sent; the API does).
- **Library**: hand-rolled HTTPS+JSON via `reqwest`. No SDK; Gmail send is
  ~30 lines of code.
- **OAuth scope**: `gmail.modify`, which uplink already asks for, covers
  sending (`gmail.send` is the narrower scope for send-only clients).
- **Threading**: a reply carries `In-Reply-To` and `References` (the
  original's References plus its Message-ID), and is sent with the
  `threadId` of the original's entry in the account, as Gmail requires
  for it to join the thread.

### 8.3 M365 — receive

- **Transport**: Microsoft Graph `GET /me/mailFolders/{folder}/messages/delta`
  per folder on first call, then `@odata.deltaLink` for subsequent calls.
  Library: `reqwest` (no Rust Graph SDK; Graph is JSON+OAuth, easy to call
  directly). Raw MIME for storage via `/messages/{id}/$value`.
- **IMAP**: not used. All M365 mail flows through Graph, which also
  serves the calendar, so one sign-in covers both.
- **Push — interim**: poll the delta link every 30 s per folder. Cheap
  under Graph throttling for a single user. The user must never see UI
  for "manual refresh"; polling is silent.
- **Push — future**: real Outlook gets push via MAPI HangingNotification
  (a long-poll over HTTPS, no public endpoint required). The closest
  third-party-accessible equivalent on M365 today is EWS Streaming
  Notifications (same long-poll pattern; it needs `EWS.AccessAsUser.All`
  on the app registration). EWS is being retired by Microsoft (last
  announced sunset October 2026, slipping). A future milestone will:
  (a) research what current first-party clients (new Outlook for Windows,
  OWA, Outlook Mobile) actually use today — possibly a Graph-native
  streaming mechanism that doesn't yet appear in public docs — and
  (b) implement whichever of EWS Streaming or its Graph successor is
  cheapest. Until then, polling is the only mechanism. See §15.11.
- **Ids**: every request asks for immutable ids (`Prefer:
  IdType="ImmutableId"`), so a message keeps its id when it moves, and
  is one `mail_entry` whatever folder it is in.
- **Threading**: use `conversationId` from each message.
- **Folders**: Graph exposes mailFolders; each folder becomes a `label`
  row marked as a folder, and an entry is in exactly one folder (§6.6,
  `SetLabels`): adding a folder moves the entry out of its other one,
  and removing its last one puts it in the Archive, as the web UI's
  archive (`e`, removing INBOX) means. The well-known folders take the
  ids of the Gmail system labels that mean the same, so the rest of
  uplink treats both providers alike: `inbox` → `INBOX`, `sentitems` →
  `SENT`, `drafts` → `DRAFT`, `deleteditems` → `TRASH`, `junkemail` →
  `SPAM`, plus `archive` → `ARCHIVE`. The Outbox, and what is below it,
  is left out, as Gmail has no counterpart. Other folders are keyed by
  their Graph id and named by their path of display names
  (`Inbox/Projects`).
- **State labels**: `isRead` false is `UNREAD`, `flag.flagStatus`
  `flagged` is `STARRED`, and `importance` `high` is `IMPORTANT`.
  Categories (`categories: ["red", "blue"]`) become labels `cat:<name>`;
  Graph has no list of them that a delta reports, so the account's
  categories are those recorded plus those seen on messages.
- **A poll round** is one step of the account's task (unlike Gmail's
  listing, §8.1): a first round, of the whole mailbox, holds up pushes
  and sends until it ends. (Open: stepping it as Gmail's listing is, if
  a mailbox this large comes up.) It follows each folder's delta link to
  its end, then settles what left: a message moved is reported gone from one folder
  and present in another, and the folders' links are read one after
  another, so a removal is only acted on once every folder has been
  read. A removal from a folder the entry was not last seen in is a
  stale one and is ignored; a removal not explained by the message
  being present elsewhere, an entry listed in two folders, or an entry
  of a folder listed anew but not in the listing, is checked with `GET
  /messages/{id}`: there, it is where it is; gone (404), it is removed.
  The round's observations carry the new delta links
  (`account.sync_state`: `{"folders": {<id>: <delta link>}}`); a round
  that changes nothing records nothing. A delta link Graph has forgotten
  (410 Gone) is replaced by listing that folder anew.
- **Push**: a pending folder change is `POST /messages/{id}/move` to the
  desired folder (by well-known name or id); read, flag, importance and
  categories are one `PATCH /messages/{id}` with their desired values
  (categories in full). A message gone meanwhile (404) is left to the
  next round to remove.
- **OAuth**: a public client of the user's own Entra ID app
  registration. v1 endpoint, `resource=` parameter, no PKCE. See §13.3.

### 8.4 M365 — send

- **Transport**: in two steps, each safe to repeat (§6.7). First the
  RFC 5322 message we built becomes a draft: `POST /me/messages` (base64
  MIME, `Content-Type: text/plain`), whose id is recorded on the send
  (`provider_draft`, by the observation `mail.send_drafted`). This has
  no effect anyone else sees, so whatever its outcome it counts as not
  sent; repeating it first looks in Drafts for our Message-ID
  (`$filter=internetMessageId eq …`), so as not to leave a second
  draft. Then `POST /me/messages/{id}/send` sends that draft. Ids are
  immutable (§8.3), so the draft keeps its id as Graph files it in Sent
  Items; sending it again (its outcome unknown, then settled as not
  sent, §6.7) refuses one no longer a draft (`isDraft`), which means an
  earlier attempt sent it: that counts as sent, so it goes out once
  only. Graph sends asynchronously (202), its copy in Sent Items not at
  once, so an unknown outcome is settled by sync as Gmail's is, not by
  reading the draft right away. Our Message-ID is kept, so the copy in
  Sent Items becomes a variant of the same logical message. Graph
  threads by the headers; there is no thread id to pass. The draft
  shows in Outlook's Drafts until sent; sync takes it for a draft, not
  for the send's copy, which is the one in Sent Items. Graph takes MIME
  messages of up to 4 MB this way; larger attachments would be added to
  the draft by an upload session (not done). Verified against the
  emulator only (§15.12).
- **OAuth scope required**: `Mail.Send`.

### 8.5 Compose

- **Body**: TipTap (StarterKit) in the browser; its HTML is the draft's
  one body, `draft.body_html`. There is no separate plain-text mode.
- **HTML sanitizer**: `ammonia` (default allowlist, which covers what
  StarterKit outputs) when the message is built, server-side. The client
  does not sanitize what it sends: the server is the boundary.
- **Outbound MIME assembly**: `mail-builder` crate (sibling of mail-parser
  from stalwartlabs): `multipart/alternative { text/plain; text/html }`,
  the plain text derived from the sanitized HTML (mail-parser's
  `html_to_text`), inside `multipart/mixed` with the attached files
  (§7.4), read from the blob store when the message is sent. The Message-ID is generated when the mail is queued,
  `<random@account.uplink>`; the Date when it is sent.
- **Replies and forwards** are prefilled by the client from the loaded
  message: `Re:`/`Fwd:` subject, recipients (reply-all drops the
  account's own address), and the body quoted. A forward takes the
  original's files along (§7.4).

### 8.6 Persistence of sent mail

When the reconciler sends a mail (§6.7):

1. Persist the assembled RFC 5322 bytes as a variant (source: the
   account), creating the logical message (its Message-ID is the one we
   generated).
2. Mark the send `sent` and broadcast.

Next sync pass, the provider's Sent copy is observed. It has the same
Message-ID but different bytes (the provider adds headers), so it is
stored as a second variant of the same message, and the message gains its
`mail_entry` for the account; the send is then `synced`, and leaves the
outbox. The same observation settles a send whose outcome was unknown
(§6.7): its Message-ID's copy in Sent means it went out.

Gmail's copy has a Message-ID of its own (§8.2), so it is stored as a
logical message of its own; the provider told which it is, and once both
are recorded (in either order), the message as sent joins it: its
variant, and whatever referred to it, move to the copy's message, under
Gmail's Message-ID, which is the one replies will refer to. The copy's
variant stays the preferred one, having been stored as the message's
first.

### 8.7 Body extraction & FTS

See §4.5. mail-parser accepts anything; a message it cannot make sense of
is still stored and listed, with empty fields.

**The query language** (`uplink-query`), after Gmail's and
notmuch's, lists conversations by their entries: every term must match
one entry. A word matches as a prefix of a word in the subject, sender,
To, Cc or body; a "quoted phrase" exactly (punctuation splits words, as
FTS5 does: `alice@x.org` is the phrase `alice x org`). Terms side by side
(or with `AND`) must all match; `OR`, `NOT` or a leading `-`, and
parentheses combine them, uppercase so that "or" is a word. Operators:
`from:`, `to:` (To or Cc), `cc:`, `subject:`, `body:`, of which `from:me`,
`to:me` and `cc:me` are the entry's own account's address;
`label:`/`tag:` (by name, in any account, ignoring case), `in:` (a label,
`drafts`, or `anywhere`), `is:unread|read|starred|important`,
`category:` (Gmail's), `has:attachment`, `account:` (id or address),
`after:`/`before:` (a date, `2026-11-09` or `2026/11/9`, UTC),
`newer_than:`/`older_than:` (`3d`, `2w`, `6m` of 30 days, `1y` of 365),
`larger:`/`smaller:` (bytes, or K, M, G of 1024), and `id:`/`rfc822msgid:`.
Trash and Spam are left out unless the query names them outside a
negation, as notmuch's excluded tags are: then the list is of them, as
Gmail's Trash is, or with `in:anywhere` of everything. Errors fail fast:
an operator uplink does not know is an error, not text (adding one must
never change what a query meant), and so is a label no account has.
The web app explains the language at `/help/search`.

**Of every kind.** The language lists rooms, to-dos and calendar events
too, by the terms they have: a term a kind lacks is false of it, and so
its negation true (`from:alice` lists only mail; `-category:promotions`
leaves every room in). `is:mail`, `is:room`, `is:todo` and `is:event`
name a kind. Words match, as in mail (by words, ignoring case, the last
a prefix unless quoted), a room's name, a to-do's title and notes, and
an event's subject, location and preview. Of rooms: `is:unread` (messages
unread), `is:mention` (one that highlights is after the fully-read
marker, §12.15: `Room.mentions`), `is:notifying` (one
notifies, by the push rules: a muted room has none), `is:dm` (a direct
chat, by `m.direct`, as the room was last synced: V0036
`matrix_room.direct`), `is:invite`, `label:`/`tag:` (its tags, §9.5, by
name or as they are, ignoring case, spaces, `_` and `-`:
`tag:lowpriority`, `tag:"Low priority"`, `tag:m.lowpriority`; a user
tag `u.work` is `tag:work`), `space:` (a space it is in, however deeply,
by name), `account:` (id or Matrix id), and dates, of its latest event.
Spaces are not listed. Of to-dos: `due:` (`today`, `tomorrow` or a
date), `is:overdue` (open, and due before today), `has:due`, `is:done`,
and dates, of its creation; done ones are left out unless the query
names `is:done` outside a negation, as Trash is. Of events: `account:`
and dates, of their start. Rooms' messages are of their rooms
(`is:room`): words match their text or an edit's (as a substring, as a
room's search does, §12.11), `from:` the sender's id or name, `from:me`
the accounts' own; `is:unread` those after the room's read marker and
not the account's, `is:mention` and `is:notifying` those that highlight
or notify; dates are of the message, and the room's terms (`tag:`,
`space:`, `is:dm`, `is:invite`, `account:`) are of its room. A label or
account no kind has is an error. `ListItems { query, after, limit }`
lists them all in one read: a page of conversations (as
`ListEntries`, a later page of them alone), and every room (most
recently active first), to-do (as listed) and event (those yet to end,
soonest first, then those past, latest first) matched, and the latest
50 messages of rooms (`FoundMessage { room, event }`), compiled to SQL
(`uplink_matrix::query::messages`), a room's terms as the rooms they
hold of. The search box at the top searches everything this way, but in
a room, where it searches the room's messages (§12.11): the search page
lists rooms, messages, to-dos and events found above the mail, each a
link to it (a message to its room); it lists again when rooms, to-dos or
calendars change, as it does when mail does. Today, which `due:` and
`is:overdue` count from, is the server's, of UTC (`uplink_query::today`)
as dates are: the user is assumed to be on UTC, until time zones are
supported (ROADMAP). Rooms, to-dos and events are few, so they are
matched as listed, in Rust (`uplink_query::matches`); mail's terms of
other kinds fold to constants in its SQL, and a query of other kinds
reads no mail.

The parsed query becomes one SQL condition on an entry. Its text terms,
those it must lack too, become one FTS5 expression where they can,
since that is one index lookup.

Speed (M10): `message_fts` has prefix indexes (`prefix = '1 2 3'`), so a
short prefix is one index lookup rather than a merge of every term it
starts. Results, like lists, are newest first, and a page is found one of
two ways, by how many candidates there are (counted first, cheaply, of
each term of the query an index finds: its text, or a label, each by its
index alone and only as far as it may be the fewest): up to 2000 of the
fewest are all sorted; more, and those of them that meet the whole
query are counted, up to 2001, since terms of many entries each may have
few in common (`in:inbox is:unread is:important`, which walking would
look through the whole mailbox for): up to 2000, and they are sorted;
more, and entries are walked newest first by an index
(`mail_entry(received_at)`, or `(account_id, received_at)` for a query
of one account) until the page is full, which many candidates make
quick. A page so costs about 2000 rows at most, or the fewest's, however
large the mailbox. Label counts are kept by the schema as entries change
(§4.3), so reading them costs nothing like the mailbox's size; a change
of labels costs about 0.25 ms more for it. The ignored test
`speed_of_search_and_listing` measures a 50,000-message mailbox: search
3–49 ms (words, `OR`, `-`, labels, dates), a page of the Inbox (of
conversations) 16 ms, All Mail 3 ms, the counts 0.03 ms.

### 8.8 Error semantics

- **OAuth refresh fails** (revoked token, password changed): set
  `account.enabled = 0`, broadcast `AccountListChanged` with an error flag,
  surface in UI as "Reauthenticate <account>".
- **Provider requests fail** (network, 5xx): retry with backoff (1s, 2s,
  4s, 8s, 16s, 30s, 60s cap). Log warning at 5 m of continuous failure.
- **Refused for rate**: Gmail's 429, or 403 with reason
  `rateLimitExceeded`/`userRateLimitExceeded`, is tried again after 1 s,
  2 s, … 64 s (Gmail sends no `Retry-After`), then fails the round,
  which the sync's backoff retries. Graph's 429 fails the round, for now;
  respecting its `Retry-After` comes with a real tenant to see it.

### 8.9 Displaying HTML mail

`LoadMessage` returns the message's first `text/html` part, sanitized
with ammonia: scripts, event handlers and forms go; `<style>` and
presentational attributes stay, since mail layout depends on them. The
client shows it in an `<iframe srcdoc>` sandboxed without scripts (same
origin only so that it can be sized to its content; popups so that links,
with `<base target=_blank>`, open in a new tab), under a CSP of
`default-src 'none'; img-src data:; style-src 'unsafe-inline'`: nothing
remote loads, so tracking pixels and remote fonts don't. The message's
own images (`<img src="cid:…">`) are inlined by the server as `data:`
URIs; `cid:` anywhere else is dropped. Messages without an HTML part show
their text.

Three layers keep mail in its place, each holding if the one inside it
fails: ammonia's sanitizing (it parses the HTML as browsers do, keeps an
allowlist of elements and attributes, and writes a new document from its
tree), the sandbox (no scripts, ever: with `allow-same-origin`, they
could lift it), and the CSP (nothing loads but what it names). The frame
also turns DNS prefetching off (`x-dns-prefetch-control`), so that the
hosts a mail links to are not looked up, telling their DNS servers it was
read. The layers are tested against hostile mail: the sanitized HTML,
parsed as the browser parses it, against a corpus of ways past them
(`uplink-email`'s `parse/hostile.rs`); and the frame, in Chromium and
Firefox (the `e2e` test), for anything that runs, navigates or reaches
the network but as asked.

Remote images are fetched by the server, never by the browser. Before
sanitizing, the HTML is parsed into a DOM (html5ever, as ammonia parses
it) and its CSS into tokens (cssparser, escapes and all), and the URLs a
browser would fetch images by are rewritten there, never in text:
`http:`, `https:` and scheme-relative URLs of `<img src>`, and of CSS in
`style` attributes and `<style>` elements (`url(…)`, `src(…)`, strings
in `image-set(…)` and after `@import`), become
`/api/remote-image/<host>?url=<URL>`, by the URL's host: a name of DNS's
letters, digits, `-`, `_` and `.`, or an IPv4 address (URLs take hosts of
`;`, `"` and more, which would end the frame's CSP, below; those of
IPv6 addresses or other characters are as other URLs). Other `<img>` sources go (but `cid:`, which
sanitizing inlines); other CSS URLs become `""` (but `data:`): relative
ones would load from uplink. The URLs are written back with their `<`
escaped (`\3c `): read with their escapes undone, a `data:` one could
otherwise close the `<style>` it is in, and what followed would be read
as HTML that was never rewritten. CSS nested deeper than cssparser reads (75
blocks) is dropped whole. Whatever the rewriting misses, the CSP blocks:
a miss is an image not shown, never one fetched by the browser.

An `<img>` likely to be a tracking pixel becomes
`/api/remote-pixel/<host>?url=<URL>` instead, which nothing serves and the
CSP never lets load: too small to see (both sides at most 2px, by its
`style` before its attributes, or one so and the other not given), or
hidden by its `style` (`display: none`, `visibility: hidden`, or an
`opacity`, `max-width` or `max-height` of 0). Only its markup tells:
fetching it to see would be what tracks. In the user's mail, where a
mail had both such images and others, those were on another host than
the others' in all but a few percent; a pixel taken wrongly is a box
left empty that was all but empty. The client says how many there are
("N likely trackers blocked"), never showing them.

Asked to show them ("Show them"), the client lets the frame load the
images of each of the mail's hosts from uplink's origin, by path
(`img-src data: <origin>/api/remote-image/<host> …`; the origin named:
Firefox does not match a `srcdoc` frame's `'self'` to the origin it
inherits); the server serves a URL only at the path of its own host, so
that a host shown cannot pass for another. A sender then learns the server's address, a
fixed user agent (`Mozilla/5.0 (compatible; uplink)`) and when the mail
was shown, but nothing of the browser: no client headers, cookies or
referrer. Remote fonts and stylesheets still don't load. Shown, the mail's
`<style>` elements end in a comment: Firefox reuses a sheet it parsed
for an earlier document of the same text and principal, with its images
as that document's CSP refused them, and would otherwise not show those
of the frame shown without them (the comment names the hosts shown).

The user may have remote images shown unasked by rules ("Always show ▾"
beside "Show them"), until asked not to (from the message's menu): all
of a sender's, by the `From:` address or its domain (exactly: not its
subdomains'), or those at a host, whoever's mail they are in, as the
menu offers them for the message, with how many of its images each host
has. Mail of hosts only some shown says "Some remote images are not
shown." Rules are lowercased, kept by uplink alone (`image_rule`) and
set by an op, `mail.always_show_images`, which undoing reverses; a
message loads with the rules it has, and events since change them.
`From:` is not authenticated, so anyone may claim to be such a sender,
and anyone may put an image of a host shown in their mail: what that
gives them is what showing would, that the mail was opened, when, and
by uplink's server. Exact hosts, rather than their sites: the user's
mail has tracking pixels on other hosts of the same site as the images
it shows (`click.` or `email.` beside `images.`) as often as on other
sites. The endpoint
(behind basic auth) fetches only from public addresses (not loopback,
private, link-local, shared, benchmarking, multicast or reserved ones,
nor IPv6 ones carrying such an IPv4 one: mapped, compatible, NAT64's or
6to4's; checked as names
resolve and at each of at most 5 redirects, with no HTTP proxy to resolve
past the check), so that a mail cannot make the server reach what only
it can. It serves only `image/*` responses, of at most 16 MB, within
30 s, with `nosniff`, `Content-Security-Policy: sandbox` (an SVG opened
as a page runs nothing in uplink's origin) and a day's private caching;
anything else is a 502.

Mail sees white paper whatever the theme: the frame element has
`color-scheme: light`, so the frame's `prefers-color-scheme` is light,
and mail with a dark mode of its own (often partial: dark text turned
light on backgrounds it leaves light) keeps to its light one. In a dark
theme (the parent's `prefers-color-scheme`, as a class on the frame's
root), the frame's page is then inverted with its hues kept
(`filter: invert(1) hue-rotate(180deg)`, as Dark Reader's filter mode),
and its images, videos and background images inverted back: mail is
written for white paper, and whatever colours it sets become their dark
counterparts. Mail designed only dark comes out light. Pictures come out a
little muted: the filter is not exactly undone. The page and the frame
carry `<meta name="darkreader-lock">`, so that the Dark Reader browser
extension, which a user may keep for other sites, does not darken them
again.

Attachments: `LoadMessage` lists the message's parts other than its body
(name, MIME type, decoded size; `inline` if the HTML body shows it), and
`GET /api/messages/{message}/attachments/{index}` (behind basic auth,
like everything) serves one as a download: `Content-Disposition:
attachment`, `nosniff`, and `Content-Security-Policy: sandbox`, so that
an HTML attachment opened anyway cannot act as uplink's page. Attaching
files to drafts: §7.4.

### 8.10 IMAP — receive

Any mail server: account kind `imap`, read by IMAP (`async-imap`, over
TLS from the start or by STARTTLS; never in the clear) and sent from by
SMTP (§8.11). Gmail and Microsoft 365 keep their own kinds, whose APIs
fit uplink better (§8.1, §8.3).

- **Sign-in**: the user name (the address unless `user` says otherwise)
  and the password `uplink password --account <id>` seals
  (`accounts/<id>.password`, §13.4), read at each sign-in so a new one
  applies without a restart; or, with `oauth` configured, an access
  token by XOAUTH2 (§13.7).
- **Folders** are labels, an entry in exactly one, as Microsoft 365's
  (§8.3), and they take the same ids: `INBOX`, and the special-use
  folders (RFC 6154, from `LIST`) `\Sent` → `SENT`, `\Drafts` → `DRAFT`,
  `\Trash` → `TRASH`, `\Junk` → `SPAM`, `\Archive` → `ARCHIVE`; a server
  without the attribute has the folder of the usual name (`Sent`, `Sent
  Items`, …, `Archive`), and one without either has it made when first
  filed in (`CREATE`), so that archiving works everywhere. Virtual
  folders that show what others hold (`\All`, `\Flagged`) are left out,
  as are those that cannot hold mail (`\Noselect`). Other folders are
  `mailbox:<name>`, named by their decoded path (modified UTF-7, the
  server's hierarchy delimiter shown as `/`).
- **State labels**: `\Seen` unset is `UNREAD`, `\Flagged` is `STARRED`;
  keywords are labels `kw:<keyword>`, but for those of clients'
  conventions (`$Forwarded`, `$Junk`, …). A message with `\Deleted` is
  gone, as other clients show it.
- **Entries** are keyed `<uidvalidity>:<uid>:<folder>`. IMAP has no id
  that survives a move (OBJECTID, RFC 8474, is not widespread), so a
  message moved gets a new key: an entry gone from one folder whose
  Message-ID is new in another moved there, and is re-keyed
  (`MailboxObserved.rekeyed`), not removed and added, so that what the
  user did to it, and its undo, stay with it. That holds for moves by
  uplink (§6.6) and by other clients, and for a folder renamed.
- **A step** checks every folder's `STATUS` (messages, UIDNEXT,
  UIDVALIDITY, and with CONDSTORE, RFC 7162, HIGHESTMODSEQ); with
  CONDSTORE, a folder as recorded has not changed and is skipped. A
  folder that changed is selected; `UID SEARCH UNDELETED` lists what it
  holds, against which the entries recorded in it are gone or kept, and
  the rest is new; flags are fetched for all, or with CONDSTORE those
  changed since the recorded mod-sequence, and recorded where they
  differ from what is observed. A UIDVALIDITY changed makes the folder's
  entries gone (and, by Message-ID, re-keyed to the new UIDs). Folders
  are looked at one by one, so a move made meanwhile could be seen half
  (gone from one, not yet in the other): a step that saw messages come
  or go takes every `STATUS` again, and if any changed, records nothing
  and is taken again. New mail
  is fetched (`BODY.PEEK[]`, so not marked read; `INTERNALDATE` is when
  it was received) 50 at a time, Inbox first and newest first, each
  step one log entry, so a first sync goes in steps between which the
  task pushes and sends, as Gmail's listing does (§8.1); the account is
  listing until the first sync has fetched all. Where it has got is not
  kept: each step finds what is still new anew. A folder's state is
  recorded once all its new mail is (`account.sync_state`:
  `{"listing": …, "folders": {<name>: {uidvalidity, uidnext, messages,
  highestmodseq}}}`); a step that changes nothing records nothing.
  Polled every 30 s.
- **IDLE** (RFC 2177): a server that has it tells of changes in the
  Inbox as they happen, on a second connection that does nothing else
  (IDLE holds its connection, and tells of the selected folder only;
  other folders wait for the poll). What it tells wakes the sync task
  (`Provider::changed`), which takes a step as for a poll; so does the
  watch's (re)beginning, as changes may have been missed meanwhile. The
  IDLE is renewed every 10 minutes (RFC 2177's limit is 29), which also
  finds a connection lost without a word; a watch that fails begins
  anew 30 s later, polling going on meanwhile. The watch begins as the
  task first waits.
- **Threading**: IMAP servers give no thread ids, so they are the
  headers': the first message a message's `References` names, else the
  one it replies to (`In-Reply-To`), else itself. Unlike Gmail's, such
  threads do not join messages by subject.
- **Push**: a message's flags are stored (`UID STORE +FLAGS.SILENT` /
  `-FLAGS.SILENT`), then, if its folder is to change, it is moved (`UID
  MOVE`, RFC 6851; without it, `UID COPY`, `\Deleted` and `UID EXPUNGE`,
  RFC 4315; a server with neither refuses moves, as the plain `EXPUNGE`
  would remove others' deleted mail too). The moved message's new key is
  found by the next step, before any further push (§6.6's task order).
  A `NO` is a refusal; an entry whose folder or UIDVALIDITY is gone is
  left for the next step to re-key or remove.
- **Connection**: one per account, kept between steps and made anew
  after a failure.

### 8.11 SMTP — send

- **Transport**: SMTP submission (`mail-send`, the stalwartlabs crate
  beside mail-parser and mail-builder), over TLS from the start (port
  465) or by STARTTLS (587), signed in as IMAP is. The envelope is the
  message's To, Cc and Bcc; the Bcc header is left out of what is
  transmitted.
- **Outcome**: a failure before the message is written (connecting,
  signing in, `MAIL`, `RCPT`) did not send it: a 5xx reply refuses it, a
  4xx or a broken connection leaves it to be tried again. A reply to
  `DATA` says what became of it; no reply, once it was written, is an
  unknown outcome (§6.7).
- **Sent**: SMTP leaves filing to the client, so once the server accepts
  the message, it is `APPEND`ed to Sent (`\Seen`), the copy that becomes
  its entry when synced (§8.6). If that fails, it was sent all the same,
  and is listed as waiting for its copy.
- **Unknown outcomes are not resent**: as the copy in Sent is uplink's,
  filed only once the server accepted the message, finding none does not
  show that it did not go out (unlike Gmail's and Graph's, §6.7). Such a
  send stays unknown until its copy turns up (a server that files
  submitted mail itself) or the user decides. (Open: a way to decide in
  the outbox: "send again" or "it went out".)

### 8.12 POP3 — receive

For servers with POP3 only, or accounts kept that way: kind `pop3`, read
by POP3 (uplink's own client, RFC 1939: `UIDL`, `RETR`, `DELE`; TLS from
the start, port 995, or by `STLS`, 110; signed in by `USER`/`PASS` or
`AUTH XOAUTH2`, as IMAP accounts are) and sent from by SMTP (§8.11).

- **Mail is downloaded**, and is uplink's from then on: an entry per
  message, keyed `uidl:<unique id>`; the maildrop's ids not among the
  entries are new, downloaded newest first, 50 a step, each step one log
  entry, as IMAP's are (§8.10). A message gone from the server (deleted
  by another client) stays. `received_at` is the first `Received`
  header's date (the last hop's), else the `Date`, else the download's
  time; threads are the headers' (§8.10). `account.sync_state` says only
  whether the first sync is still listing: what was downloaded is the
  entries.
- **Deleting from the server** is the account's choice
  (`delete_after_download`, off by default, so that other clients still
  see the mail): each step, once its downloads are recorded, `DELE`s
  every message of the maildrop already downloaded, which the server
  deletes as the session ends (`QUIT`); a session that fails before it
  deletes nothing, and the next step deletes again.
- **Labels are uplink's**: POP3 has no folders or flags to keep in step,
  so the account has Gmail's system labels (`INBOX`, `UNREAD`, `STARRED`,
  `SENT`, `SPAM`, `TRASH`; none a folder, so archiving takes `INBOX`
  away); new mail is in `INBOX`, unread. A change takes effect as it is
  pushed (§6.6), touching nothing at the server.
- **Sent mail** is filed by uplink: once the SMTP server accepts it, its
  entry (`sent:<Message-ID>`, labelled `SENT`) is recorded, which settles
  the send (§8.6). An unknown outcome waits for the user, as an IMAP
  account's does (§8.11).
- Polled every 60 s: each poll signs in anew, which providers may limit.
  A server without `UIDL` cannot be synced (the step fails, saying so).

### 8.13 JMAP

Any mail server that speaks JMAP (RFC 8620, core; RFC 8621, mail; e.g.
Stalwart, Fastmail, Cyrus): kind `jmap`, read, filed and sent from by
JMAP alone. The client is uplink's own, on the server's shared `reqwest`
one, as Gmail's and Graph's are: `jmap-client` (Stalwart's crate) makes
an HTTP client per request, cannot trust another root, and keeps the
credentials it was built with, which an OAuth token outlives.

- **Session**: `session_url` (`https:` only; most servers' is
  `https://<host>/.well-known/jmap`) names the API, blob and event
  source URLs and the mail account (its primary one); a server without
  the core, mail and submission capabilities is refused. The session is
  fetched anew once a response's `sessionState` differs from it.
- **Sign-in**: the user name (the address unless `user` says otherwise)
  and the password `uplink password` seals, by HTTP's `Basic`; or, with
  `oauth`, an access token, by `Bearer` (§13.7). What signs in is kept
  until a 401, after which the next request reads the password anew, or
  refreshes the token.
- **Mailboxes are labels**, as Gmail's are (§8.1): an email may be in
  several, and adding one leaves the others. Those of a role take the
  ids of the Gmail labels that mean the same: `inbox` → `INBOX`, `sent`
  → `SENT`, `drafts` → `DRAFT`, `trash` → `TRASH`, `junk` → `SPAM`; the
  rest are `mailbox:<id>` (by the server's id, which a rename keeps),
  named by their path (`Parent/Child`). Virtual mailboxes (`all`,
  `flagged`) are left out. Trash and Spam are as Gmail's: trashing adds
  `TRASH` and leaves the Inbox, where other clients then show it too.
- **The Archive is All Mail**, not a label: JMAP keeps every email in
  some mailbox, so one in the Archive (role `archive`) alone is in no
  label, as an archived Gmail message is. A push that would leave an
  email in no mailbox puts it in the Archive (made, by `Mailbox/set`, if
  the server has none); one that gives an email in the Archive another
  mailbox takes it out. Archiving (removing `INBOX`) therefore files the
  email in the Archive, and its undo takes it out again.
- **State labels**: `$seen` unset is `UNREAD`, `$flagged` is `STARRED`;
  other keywords are labels `kw:<keyword>`, but for those of clients'
  conventions (`$answered`, `$forwarded`, …).
- **Entries** are emails, keyed by their ids, which JMAP keeps as an
  email moves; threads are the server's (`threadId`; Stalwart's, as
  Gmail's, join a reply only of its subject).
- **A step** is one request: the mailboxes (all of them, each step, as
  they are few; the labels are recorded where they differ), and the
  emails' changes since the recorded state (`Email/changes`, at most 50),
  with the changed emails' mailboxes and keywords (`Email/get`, by
  back-reference). An email new to uplink, created or updated, is
  downloaded (its blob, 4 at once) and recorded as received at its
  `receivedAt`; one it has is relabelled; one destroyed is removed. More
  changes than a step takes are taken by the next at once. A state the
  server can no longer calculate changes from (`cannotCalculateChanges`)
  lists the emails anew. A step that changes nothing records nothing.
- **Listing**, the first sync or a state lost, is Gmail's (§8.1): the
  state as it began is recorded (`account.sync_state`:
  `{"emails": <state>, "listing_before": <entry>}`), the emails are
  listed newest first (`Email/query` by `receivedAt`, 500 a page, each
  after the last one's id, by `anchor`, so that emails gone meanwhile
  shift nothing), the new ones fetched 50 a step, and the ones known
  before it began relabelled; at its end, what was not listed is
  removed and the state recorded (`{"emails": <state>}`), from which the
  polls after it record what changed meanwhile. A listing interrupted
  resumes, listing anew and fetching only what it has not; so does one
  whose anchor is gone (`anchorNotFound`).
- **Push**: each email's keywords and mailboxes are patched (`Email/set`,
  `keywords/<k>` and `mailboxIds/<id>`, so that what other clients
  changed meanwhile stays), its mailboxes read just before for the
  Archive's rule; a `notFound` is left for the next step to remove, any
  other error is a refusal.
- **Event source** (RFC 8620 §7.3): a connection of its own tells of
  changes to emails and mailboxes as they happen, waking the sync task
  (`Provider::changed`), which takes a step as for a poll; so does the
  watch's (re)beginning. It asks for a ping a minute (Stalwart pings
  every 30 s at least), and one silent for 5 minutes is taken for lost;
  a watch that fails begins anew 30 s later, polling going on meanwhile
  (every 30 s). The watch begins as the task first waits. (Stalwart
  tells nothing of an `Email/import`; of mail delivered, it does.)
- **Sending** (§6.7): the message is uploaded and imported into Drafts
  (`Email/import`, `$draft` and `$seen`), which is recorded
  (`SendDrafted`), then submitted (`EmailSubmission/set`, the envelope
  the server's: From, and To, Cc and Bcc; the server takes Bcc out of
  what it delivers, and the copy keeps it), from the identity of the
  account's address, else of its domain (`*@domain`), the copy moved
  from Drafts to Sent as the server accepts it (`onSuccessUpdateEmail`):
  the draft is the copy (`SentCopy::Id`). Before a draft recorded is
  submitted again, its submissions are asked for (`EmailSubmission/query`
  by `emailIds`): one there means it went out. A failure before the
  submission did not send it; a submission refused (`notCreated`), or a
  draft gone, is a refusal; an answer lost is an unknown outcome,
  settled by the copy in Sent once synced, or else sent again after 5
  minutes, which finds the submission if it was made (`FILES_SENT`).
- **Connection**: HTTP's, kept by the client; a request may take a
  minute, a blob ten.

---

## 9. Matrix pipeline

### 9.1 SDK & shape

`matrix-sdk` 0.19 (matrix-rust-sdk, the crate Element X uses), with E2EE,
one client per account. Its stores (state, crypto, event cache) are
SQLite under the account's directory (`matrix/<id>` under the data directory), encrypted with a passphrase
derived from the master key (`MasterKey::derive("matrix-store:<id>")`).
The SDK does not keep the session (access token, device id); uplink seals
it in `session.sealed` there (§13.4).

The account's sync task (`uplink_matrix::sync`) runs two loops on the
client:

- **Receiving.** `/sync` (v2, §15.6) long polls of 30 s, from the token
  recorded in `account.sync_state`, passed explicitly: the SDK's own
  stored token is not used, so what uplink recorded and where it syncs
  from always agree. A response that changes something uplink stores is
  recorded as one `matrix.synced` observation (rooms upserted, events
  added, own receipts applied, the token updated) in one transaction.
  Responses that change nothing uplink stores (empty long polls, others'
  receipts) are not recorded, to keep the log quiet; the next sync
  starts from their token all the same. After a restart, sync resumes
  from the recorded token, and events already stored are skipped. The
  SDK decrypts as it syncs. The filter lazy-loads members, drops
  presence, and keeps receipts and typing of the ephemeral events.
  Who is typing (but the account), and that others sent read receipts,
  are pushed as notices (§5.5) once the response is recorded.
- **Sending.** On every user op (the hub), or every 30 s: sends queued
  messages (§9.3) and pushes the read marker (§9.5).

### 9.2 Persistence

Every timeline event becomes a `matrix_event` row: its type, for
`m.room.message` its `msgtype` and plain-text `body`, the event JSON
(decrypted, if it was encrypted and could be), whether it was encrypted,
and the sender's display name in the room at the time.

**Nothing is dropped:** the timeline shows text message types (`m.text`,
`m.notice`, `m.emote`) as text, undecryptable events as such, and every
other event — other message types, state events, types uplink has never
heard of — as its raw JSON, collapsed under its type. This is the "fail
open" invariant from the ROADMAP: don't drop, don't crash, display
verbatim.

The first sync has no token, so the homeserver answers at once, but
building an answer with every room can take it minutes on a large
account: its request is allowed 10 minutes (the SDK times a sync out
30 seconds after its long poll, and the first sync's long poll is set to
that).

uplink keeps where it synced to (`account.sync_state`) and names the
token in every sync; the SDK's own copy is forgotten when the session is
restored. The SDK skips a response whose `next_batch` is the token it
has, taking it for one it processed; but a first sync that ends where the
last one did (nothing happened since, as after pairing) is not that one,
and has every room.

**Relations.** Each event's relation is stored with it, from its JSON
(V0025): `m.relates_to`'s `rel_type` and `event_id`, a reply's
`m.in_reply_to`, a redaction's `redacts` (in the content from room
version 11, at the top level before), and whether it is redacted. The
timeline folds into a stored event what is of it, as Element shows
them, and does not list them on their own:

- **edits** (`m.replace`, messages by the same sender with
  `m.new_content`): an event carries its latest, by `origin_server_ts`
  then event id; an edit is not edited itself. Its other edits are
  listed by `ListEdits`;
- **reactions** (`m.reaction` with `m.annotation`): an event carries
  them by key, in the order first reacted with, each sender once, with
  the reaction's event (redacting it takes the reaction back);
- **redactions**: the event redacted is redacted in storage too, by the
  room version's rules (ruma's, the SDK's fallback version if it does
  not know it; an encrypted event's payload, which may not be canonical
  JSON, loses its content), with `unsigned.redacted_because`, as the
  homeserver would serve it. Redacted relations stay folded: a
  redacted edit or reaction no longer counts. A reaction that came
  redacted (and so no longer says what it was of) is not listed either.

Relations of events not stored are listed, as their raw JSON, since
nothing is dropped. A reply carries the event it answers, if stored,
with its latest edit, for the reply to show. A sync that adds or
redacts relations names the events they change as `TimelineChanged`,
which clients load again (`ListRoomEvents`). Pages count only the
events listed; so does the unread count, which skips redacted messages
too.

**Threads.** A message of a thread (`m.thread`) whose root is stored is
the thread's, not the main timeline's (`thread_root` names the root):
the thread lists it (`ListThread { room, root }`: the root, and its
messages in timeline order), and neither pages nor the unread count
count it. The root sums its thread up (`thread`: how many messages,
and the latest, as a reply shows what it answers). A thread message's
fallback reply (`is_falling_back`, to the thread's latest, for clients
that do not know threads) is no reply; a reply within the thread is.
Of a thread whose root is not stored (not yet fetched), messages are
listed in the main timeline, marked as of a thread; once the root is
stored, `TimelineChanged` names them, and clients find them the
thread's. A thread's root changes (`TimelineChanged`) as messages are
added to the thread, and as its latest is edited or redacted.

**HTML.** A message's HTML `formatted_body` (and its latest edit's) is
listed sanitized, as mail's is (§8.9), by the server: the tags and
attributes the Matrix spec suggests clients show, and no others; links
open in a new tab; a code block keeps only its `language-*` class; a
reply's fallback (`mx-reply`) is dropped, as the client shows the reply
itself. Images show the homeserver's media only (`mxc:`), as the server
serves them (§9.8); nothing remote loads. The HTML is in the app's page,
not a frame: what keeps it there is the sanitizing, and the page's CSP
(§13.1). Of uplink's own URLs, it has only the event's media's, of
well-formed `mxc:` URIs (a server name and an id of the spec's
characters; not `.` or `..` for a server name, which ruma takes, as of
DNS's characters): a path the browser takes as it is. Otherwise
dot-segments, which it resolves (`%2e` too), would reach anything of
uplink's, its fetches of mail's remote images most of all: a message
would have the server fetch whatever it names as it is shown. The
client builds the paths of the media events send, of avatars and of
previews' images so too. `html/hostile.rs` checks it against hostile
messages, and the `e2e` test in Chromium and Firefox.

The timeline is ordered by `pos`, not by id: ids are in the order
events were stored, and events fetched later may be earlier. A synced
event comes 2^20 after the room's last; read markers and receipts
compare by position too. A sync whose timeline leaves events out
(`limited`, as the first sync does) records a gap before the first
event it stores; so did V0028, before the first event of each room,
unless it is `m.room.create`. Pages of the timeline list the gaps just
before their events (and after the last, if it is the latest), which
the client shows as rows, and `FillGap` fills: the server asks the
homeserver for the context of the event after the gap, for a token
before it (pagination tokens are not kept), and for the 50 events
before that (`/messages`, lazily loading members, whose events give the
senders' names then). The observation `matrix.filled` stores those not
stored yet, newest first, until one that is, which closes the gap; they
go just before the gap's event, one apart (so no position ever
changes; a gap holds 2^20 - 1 events). A new gap before the oldest
replaces the old, unless the gap closed or the room's start was
reached. It logs `TimelineFilled` (the listed events added, and the
gap left), and `TimelineChanged` of the events they relate to.
Events that arrive undecryptable are decrypted once their keys come
(the observation `matrix.decrypted`, which updates the stored event in
place, logging a `TimelineChanged` of it and of what it relates to, as
it may turn out an edit or a reaction, no longer listed). A task by the
sync task's side retries: the events of a Megolm session when the SDK
receives its key (from another device, an import or the key backup);
every event stored undecryptable on start, when the key backup becomes
usable (as on recovering, §9.4), and when the SDK's key stream lags;
and the events a sync records undecryptable, as soon as it is
recorded. A failed decryption has the SDK look in the key backup
(`BackupDownloadStrategy::AfterDecryptionFailure`), if the device has
the backup's key. Events redacted meanwhile stay as they are.

### 9.3 Sending

`matrix.send` (`SendMatrixMessage { room, content }`) queues a send in
`matrix_send`, wanted (§6.7). Its content (`MatrixContent`, stored as
JSON since V0026) is one of:

- **text**, in Markdown: sent as `m.text` (or `m.emote`, for `/me`) with
  the plain body, and, if Markdown formats it, `formatted_body` in HTML
  (ruma's CommonMark, as Element does). Its links to users
  (`[name](https://matrix.to/#/@user:server)`, which Element shows as
  pills) mention them: the plain body has their names, and
  `m.mentions.user_ids` the users (intentional mentions; `{}` with
  none). Or else (`markdown: false`, `/plain`; V0030 marked the earlier
  sends Markdown) it is sent as it is, mentioning no one by links.
  Either way, `@room` as a word of its own (not of a user id,
  `@room:server`) mentions the whole room (`m.mentions.room`), which
  notifies its members as its power levels let the account. It may reply to
  a stored event (`m.in_reply_to`, without the fallback quote, which the
  spec no longer asks for), mentioning its sender, or edit one of the
  account's own messages (`m.replace`, with `m.new_content`, which
  mentions whom it does, and `* ` before the body for clients that do
  not know edits; the edit itself mentions no one anew). It may be of a
  thread (`thread`: the root, a stored event of no thread; not an
  edit): `m.thread`, replying to the root or a message of the thread if
  it does, else to the thread's latest message as a fallback
  (`is_falling_back`), as the spec asks;
- a **reaction** to a stored event (`m.reaction`, `m.annotation`);
- a **redaction** of a stored event not yet redacted (deleting a
  message, or taking a reaction back), through the redaction endpoint,
  which is deduplicated by transaction id too;
- a **file**, uploaded first to uplink's blob store (`POST /api/blobs`,
  as drafts' are, §7.4) with its name and MIME type: the SDK's
  `send_attachment` uploads it (encrypted, in an encrypted room) and
  sends it as `m.image`, `m.video`, `m.audio` or `m.file`, by its type,
  maybe as a reply. A retry uploads it again (only the event is
  deduplicated), which leaves the earlier upload unused;
- the room's **name** or **topic** (`m.room.name`, `m.room.topic`; empty
  takes it away), as state events. Setting state is idempotent, so a
  retry is safe;
- a **pin** or an **unpin** of an event (`m.room.pinned_events`): of the
  events pinned as the client has them when it is sent, so that others
  pinned meanwhile stay; the pin goes last. One pinned already, or not
  pinned, sends nothing. Only a stored event that is not deleted is
  pinned; any is unpinned;
- a change to a **member**: inviting, kicking (of one invited, revoking
  the invitation), banning or unbanning a user, with a reason (but an
  invitation), through the membership endpoints. These say no event id:
  sent, the send is synced at once (V0029 allows a sent send without
  one), and the membership event shows when synced. They have no
  transaction id, so after an interrupted send that went out, the retry
  may be refused as made already.

Whether the account may make a change to the room is the homeserver's to
say: a refusal needs a decision, as any.

Events are named by their Matrix ids, and must be stored in the room.
The room's timeline shows it at the end,
with its state (`ListTimeline`), until it is synced back, and the
outbox lists it with the account's. Cancelling it (its undo, or
`CancelMatrixSend`) drops it while queued or refused; once claimed, it
makes it unwanted, settled by the outcome. The sending loop claims a
message (`sending`, so that a cancel knows it can no longer be fulfilled
at once) and sends it with transaction id `uplink-<id>`:

- accepted: `sent`, with its event id; `synced` once that event is synced
  (or at once, if sync delivered it first). If it was cancelled
  meanwhile, it is taken back (below);
- refused (4xx other than 429): `refused`, with the homeserver's reason;
  it needs a decision, and cancelling drops it. Cancelled meanwhile, it
  is dropped at once;
- anything else, including a send interrupted by a restart: queued
  again and retried with backoff — the homeserver deduplicates the
  transaction id, so a retry cannot duplicate the message. Cancelled
  meanwhile, it is dropped instead.

Cancelled once sent (as undoing it usually is, there being no send
delay), a send is taken back as nearly as Matrix allows
(`MatrixContent::retractable`), however long ago it was sent: a message,
a reply, a file or a reaction is redacted (others may have read it, or
been notified, and the room shows it deleted); an edit is redacted, which
shows the message as it was before it; a pin is reversed (and one that
changed nothing needs nothing). The sending loop does it as it sends,
after the queued messages: it claims the send (`taking_back`), sends
what takes it back with transaction id `uplink-<id>-back`, and marks it
`taken_back`, reconciled. Failing for a passing reason, or interrupted,
it is tried again; refused, the send is `sent` again with the reason, and
needs a decision, as does one that can't be taken back: a deletion, or,
not yet, a name, a topic or a change to a member. Keeping it sent
(`KeepMatrixSent`, or undoing the cancel) resolves it. Undoing the cancel
before the take-back is claimed wants it sent again, which it is; once
claimed or done, it sends the message anew, as a send of its own (another
event, notifying again): the taken-back row stays, so that it knows what
to send.

### 9.4 Cross-signing pairing

`uplink matrix pair --account ID` (run as the service user) logs in with
the account's password as a new device ("uplink (ID)"), checks that the
homeserver logged in the configured mxid (it may spell it otherwise, in
another case), syncs once without rooms to upload the device's keys
(quickly, however many rooms the account has), and seals the session. A
pairing that fails before that logs its device out and removes the
account's state directory, so that it can be retried. It then asks for
the account's recovery key or Secret Storage passphrase: with it,
matrix-sdk's `Recovery::recover` imports the cross-signing keys (and the
key backup key) from Secret Storage, so the device signs itself and the
account's other devices trust it; left empty, the device stays
unverified. Typed at a terminal, both are read without echo; otherwise
a line each from standard input, for scripts. A running uplink picks
the paired account up within a minute (its sync task retries restoring
the session with backoff). To pair again, delete the account's state
directory: its stores belong to the old device.

Pairing is a CLI flow because it creates the device, once. What follows
is the SPA's, on an account's Security page (§12.12), answered by the
account's client and not logged (they are the account's state, not
uplink's): `GetMatrixSecurity` (uplink's device, whether the account's
cross-signing signed it, whether it has the cross-signing keys, the key
backup's and recovery's state, and the account's devices with their
trust); `RecoverMatrix`, which unlocks Secret Storage as pairing does,
for a device paired without; `ImportRoomKeys` of an export (an uploaded
blob) another client made, with its passphrase, answering how many
keys were new; and exporting the device's room keys, encrypted with a
passphrase, over HTTP (`POST /api/matrix/:account/room-keys`, the
passphrase as the body, as a download: a POST, as it reads a secret).
The sync task watches the backup's, recovery's, devices' and
identities' streams, and pushes `SecurityChanged` notices (§5.5).

A device paired without the recovery key verifies itself with another
of the account's devices, by comparing emoji (SAS), as Element does:
`VerifyMatrixDevice` asks a device, or every device of the account
(whichever answers), and `AnswerVerification` accepts one asked of
uplink, confirms that the emoji match or says they do not, or cancels.
Only verifications with the account's own devices are answered:
requests of other users are ignored. The verifications are the SDK's;
the account's entry in `Clients` keeps their requests (those ended are
dropped when another begins), listed in `MatrixSecurity`. Each is
watched while it lasts: comparing starts once a request is ready, a
comparison the other device starts is accepted, and every change pushes
a `SecurityChanged` notice. Once done, a device with the cross-signing
keys signs uplink's, and the SDK asks it for the account's secrets,
which a verified device gives.

### 9.5 Rooms list / unread counts

- A room's unread count is computed locally: messages (and undecryptable
  events, which nearly always are messages) from others after the read
  marker. The homeserver's notification counts are not used, since they
  could not follow an undo. Of those, of the main timeline and threads,
  a room counts those that notify (`notifying`) and highlight
  (`highlighted`: mention the account, or a keyword), by the account's
  push rules as its client evaluates them on sync (the SDK's push
  actions, of an encrypted event once decrypted, also late; V0032:
  `matrix_event.notify`, `highlight`; events stored before, and
  backfilled ones, are neither). The nav does not count them as
  Element does: a room is bold while unread, and its mark (§12.14)
  counts its messages that notify only while it is in a `count`
  bucket.
- **Browser notifications**: once the browser lets uplink notify (a nav
  item asks it to), the web client notifies when a room's messages that
  notify grow while the tab is hidden or another page shows: the room's
  name and its latest message from others (among its latest 10 events),
  one per room, a click opening it. Those unread when the rooms are first
  loaded are not news.
- The read marker is desired state (§6.6). Showing a room, in a visible
  tab, marks it read up to the latest message from others wholly on
  screen (`MarkRoomRead`, quietly: no notice, and as shown, §12.15). The
  sending loop pushes an `m.read` receipt for it. The fully-read marker
  (§12.15) is kept and pushed alike, as `m.fully_read`.
  Receipts only move forward, so undoing a mark-read moves the marker
  back in uplink only.
- The account's own receipts from other devices move the marker forward,
  when past the latest receipt uplink sent or saw (so the echo of its own
  receipt does not undo an undo).
- Spaces nest the rooms they list (by `m.space.child` events with
  `via`), as Element shows them: a joined space has its joined subspaces,
  in its `order` (those without one last, then by room id), then its
  rooms, by activity; at the top are the spaces in no other, in the
  user's order, then the rooms in no space. A room may be in several
  spaces. Spaces that are only in each other go at the top; a space is
  never nested in itself. `m.space.parent` is not used: a child's claim to
  a parent is not the parent's listing. Spaces are not links, as their
  overview is deferred; an invited space is a room to join.
- The user's order of the top-level spaces is MSC3230's, as Element keeps
  it: each space's `org.matrix.msc3230.space_order` room account data
  holds an `order` (a string of `\x20`–`\x7E`, at most 50 long; others
  are ignored). Spaces with one go first, by it, character by character;
  then those without; ties by room id. It is desired state (§6.6):
  dragging a top-level space in the nav dispatches `OrderSpaces`, a list
  of `{ space, from, to }`, which the client computes as MSC3230
  recommends, changing as few spaces as it can: the space moved takes an
  order between its new neighbours' (as base-95 numbers, the shorter
  padded with spaces), and those before it without one take one too;
  where none fits between, its neighbours move as well. A space whose
  order is no longer `from` makes it a conflict; its inverse swaps `from`
  and `to`. The sender loop sets each order asked for (no order is empty
  content), and records it, or the homeserver's refusal, which waits in
  the outbox for a decision (undoing the op); sync settles an order asked
  for once it sees the homeserver has it, however it came about.
- Avatars: a room's `m.room.avatar`, or of a direct chat without one, the
  other user's, as the SDK's store has them when the room is synced (so a
  change to `m.direct` shows when the room next changes). The server
  serves a 64 px square thumbnail of a room's current avatar at
  `/api/rooms/:room/avatar/:server/:media` (the path names the
  `mxc://` URI, so it is cached for good), fetched by the account's
  client, through the SDK's media cache, as authenticated media; only
  PNG, JPEG, GIF and WebP, by their signatures. A room without one, or
  whose thumbnail fails, shows its name's first letter on one of six
  colours, by its id, as Element does.
- Others' read receipts are the SDK's, as its store keeps them:
  `ListReceipts` gives each joined member's (but the account's; those
  the client knows, as members are lazily loaded) latest `m.read`
  receipt, unthreaded or else of the main timeline, for stored events.
  Clients show a member on the last listed event at or before it, as
  Element does. `SetTyping` records whether the account is typing in
  the room, and answers at once; a task per room sends the homeserver
  the latest wanted, each time it changes, so a stop never overtakes the
  start before it (§5.6). A failure is logged, not answered: typing is of
  the moment. The SDK makes the notice last 4 s: the web client renews it
  every 3 s while text is typed, and stops it when the box is emptied or
  sent.
- `ListMembers` lists the room's members as the account's client has
  them (loading them all first, as sync loads them lazily): joined,
  invited, asking to join, left or banned, with their power levels (none
  for a creator of a room of version 12, above any), whether the
  account may kick and ban each (its power is above theirs, and the
  room's levels allow), and whether it may invite and change the name
  and the topic. It is not logged; clients load it again as events
  arrive.
- **Tags**: a room's `m.tag` room account data, as Element keeps it:
  `m.favourite`, `m.lowpriority`, and the user's own, `u.` and a name
  (others' tags, as other clients may set, are kept and may be removed,
  not added). V0035: `matrix_room_tag`, as `entry_label` is of mail
  (§6.6): `observed` as the client's store has it on sync, `desired` as
  asked, `desired_by` the op while they differ, `refused` the
  homeserver's reason. `ModifyRoomTags { rooms, add, remove }` applies
  `SetRoomTags`, a list of `{ room, tag, desired }`, skipping those in
  effect so its inverse reverts exactly them; tagging a room a favourite
  untags it of low priority, and the other way, as Element and the SDK
  do (implied changes undescribed). The sender loop sets and removes each
  tag asked for (`PUT`/`DELETE .../tags/{tag}`, without an order); a
  refusal waits in the outbox, listed by op, for a decision (undoing the
  op). Sync settles a tag had as asked, however it came about; others'
  changes show unless one is asked for. `Room.tags` lists those shown
  (as asked), sorted.
- **Pinned events**: a room records, as its client has them on sync,
  the events pinned (`m.room.pinned_events`, in its order) and whether
  the account may pin (by the room's power levels; of a room invited
  to, the default levels'), V0033: `matrix_room.pinned` (a JSON array),
  `may_pin`. `Room.pinned` lists them; `ListPinned` gives those stored.
- A room is recorded when a sync brings state changes too, not only
  timeline events, so that a name, avatar or space's children changed in
  a gappy sync are seen.

### 9.6 What we deliberately don't do

- No verifying other users, nor by QR code (§9.4). No setting up
  cross-signing, Secret Storage or a key backup, nor renaming or signing
  out devices: another client does.
- No deleting others' messages: one deletes one's own only (members are
  removed and banned, §12.11).
- No files sent to a thread: its box sends text.

### 9.7 Joining and leaving

Membership is desired state (§6.6). `ChangeMembership { room, change }`
(join an invitation; leave a room, or decline an invitation) records
`matrix_room.desired`, and the op that asked (`membership_by`); the
account's sender loop asks the homeserver (`join_room_by_id`,
`Room::leave`) and records the outcome as an observation: the
membership, or the refusal in `membership_error`. A refused change stays
asked for, and is not asked again: the outbox lists it as needing a
decision (§6.6), which undoing the op makes, keeping the membership as
it is (an op changes one room, so its undo is that decision). Sync
clears `desired` once it sees that membership, however it came about.
Rooms left are not listed, unless being joined again.

Undo goes back to how the room was, as far as possible: a change not yet
made is cancelled; one made is reversed (a joined room is left; a room
left is joined again, which the homeserver refuses unless the room lets
one in). The inverse decides which when it is applied, since that is
when it is known, and is refused if the membership changed otherwise;
its own inverse redoes. `tests/conduit.rs` joins an invitation, undoes
(leaves) and redoes (joins again), and declines another.

Rooms new to uplink are joined at once, as requests of the moment (they
log nothing; sync records the room, and leaving it is the op above):
`CreateMatrixRoom` creates one (named, with a topic, inviting users,
encrypted or not; private, or public under an address), `StartDirectChat`
opens the account's joined direct chat with a user, or creates one
(encrypted, marked direct in `m.direct`), and `JoinMatrixRoom` joins by
address or id. Each answers `MatrixRoomJoined` with the room's Matrix id
once the homeserver has; its refusal (as of an address that does not
exist) is `Invalid`, with its reason. The client opens the room once
sync lists it. Creating a room and starting a chat are not sent again on
a new connection (a second would be made); joining is.
`ResolveRoomAlias { account, alias }` (not logged) answers
`RoomAliasResolved` with the Matrix id of the room an address is of, as
the account's homeserver resolves it; an address that does not exist is
`Invalid`, as above.

### 9.8 Media

The server serves the media events name, fetched by the account's
client through the SDK's media cache (as authenticated media, and
decrypted, of encrypted rooms):

- `/api/rooms/:room/events/:event/media/:server/:media` — the media
  `mxc://server/media`, if the event (or one of its edits) names it: as
  the file it sends (`url`, or an encrypted `file`), its thumbnail
  (`info.thumbnail_url` or `thumbnail_file`), or an image of its
  `formatted_body` (custom emoji, inline images). Only those, so that
  the web client cannot have the server fetch whatever it names. Its
  type is the event's `info.mimetype` (a thumbnail's,
  `thumbnail_info.mimetype`); an image named without one is known by its
  signature. Only images the browser decodes (PNG, JPEG, GIF, WebP,
  AVIF; not SVG, a document), video and audio are served `inline`;
  anything else is an `attachment`, named by the event's `filename` or
  `body`. Sandboxed (`Content-Security-Policy: sandbox`, `nosniff`), and
  cached for good: media never change.
- `/api/rooms/:room/events/:event/thumbnail/:server/:media` — the
  homeserver's thumbnail of such media, scaled to fit 800×600, as
  Element asks for: of plain media only, since the homeserver cannot
  see encrypted ones (their senders attach thumbnails of their own).
- `/api/rooms/:room/members/:user/avatar` — a 64 px thumbnail of a
  member's avatar, as the SDK's store has it; cached for an hour, as
  members change their avatars. A member without one is `204 No
  Content`, not an error (which browsers log): the image fails to load,
  and the web UI shows their initial.

**Link previews.** `PreviewUrl { room, event, url }` (not logged) asks
the account's homeserver for its preview of a web page (authenticated
`/preview_url`, which fetches the page and caches it): what the page
says of itself (OpenGraph's title, description and site name, `None` if
neither of the first two), and its image, which the homeserver uploads.
The homeserver's answer is final, an error too: the SDK does not retry
it (it would, for up to a quarter of an hour, of a 5xx). Only of an `http(s)` URL that the message (or one of its edits) has in
its `body` or `formatted_body`, so that the web client cannot have the
homeserver fetch whatever it names; and not in encrypted rooms, as
Element by default, since it would tell the homeserver of the links.
`/api/rooms/:room/events/:event/preview/:server/:media?url=` serves the
homeserver's 800×600 thumbnail of the preview's image, if it is that
URL's preview's (asked anew: the homeserver caches it). The web client
previews, under a message (not a notice, which bots send), up to two of
the links it shows, but those to `matrix.to`: of its HTML, its links'
targets (its `body` may be in a bridged network's markup, as Slack's
`<http://dlang.org|dlang.org>`); else the URLs in its text. A page that
cannot be previewed shows none.

---

## 10. Calendar pipeline

### 10.1 M365 only, read-only

- **Transport**: Graph `GET /me/calendarView/delta` of the default
  calendar, with the M365 mail sync's `Graph` (one per account, §8.3) and
  its `Prefer` header, so times are UTC and pages up to 100. The window
  is from 31 days ago to 366 days ahead. A delta link holds its window,
  so once the window's start lags where it would be now by over 7 days
  the calendar is listed anew for the window of now; a 410 for the delta
  link does the same.
- **Polling**: every 5 minutes. A round that finds no changes records
  nothing, keeping the old delta link (good until it changes).
- **Recorded as** a `calendar.observed` log entry (an observation, §6;
  not undoable): the first listing (or a new window's) replaces the
  account's events, later rounds upsert changed instances and delete
  `@removed` ones, and the `calendar_sync` state is written in the same
  transaction. Its `calendar_changed` event tells clients to query again.
- **Recurring events**: a calendar view expands series itself — each
  occurrence is an instance with its own id and `seriesMasterId`, an
  exception (`type: exception`) carries its changes, and cancelled
  occurrences are left out. So uplink stores instances as Graph lists
  them, with no client-side RRULE expansion: the SPEC's earlier plan
  (store masters with `rrule`, expand with the `rrule` JS library) would
  duplicate Graph's expansion and its exceptions handling, and calendar
  view delta takes no `$select` to ask for masters anyway.
- **Storage**: the instance's Graph JSON in `calendar_event.raw`, and the
  fields shown in columns (§4.3): among them its status, which the
  week shows (`isCancelled`, `showAs`, `responseStatus.response`;
  V0046). The earlier plan to keep `.ics` files
  is dropped: Graph has no iCalendar export of an event.
- **Query**: `list_events {from, to}` (ms) answers every account's
  events overlapping the range (at most 400 days), by start;
  `get_event {event}` all there is of one, read from its JSON as asked:
  organizer, attendees and their answers, the body (HTML sanitized as
  mail's, §8.9), the online meeting's join URL, its Outlook web link,
  and the JSON itself, for what uplink does not show.

### 10.2 Display

`/calendar/day/<date>`: a day, as a week's column; the calendar opens
in it on a phone (narrower than 700px), which a week's seven columns
would not fit. `/calendar/week/<date>`: the week from Monday, a column a day, all-day
events in a row above the hours, timed events at local times and side
by side where they overlap. The days' heads and all-day rows line up
across the week (a grid's rows) and stay as the hours scroll; the
hours are ticked in a column on the left; today is tinted, and the
time a line across it. The hours are ticked in the browser's time
zone and, in columns left of it, in those of `calendar.time_zones`
(§3.2), each headed by its label: as Outlook's extra time zones, for
the office's hours and UTC. (A zone's ticks are as of the week's first
day, or today: a change of daylight saving time within the week is not
followed.) Config changes apply at once (`CalendarConfigured`). A week opens at 08:00, this week at an hour
before now. `/calendar/agenda/<date>`: the days with events in the
fortnight from the date. Recurring events are marked ↻, changed
occurrences ↻*. As Outlook shows them, an event cancelled (but still
on the calendar) is struck through, one declined faded, one not yet
answered or tentative hatched, one shown as free hollow; uplink hides
none of them. An event is a link:
`/calendar/<view>/<date>/<event>` opens it beside the view (over it,
on a phone), with what `get_event` has, and a button to join its
online meeting; `Esc` closes it. Home's agenda and search's events
link to it so. `n` and `p` open the next and previous event on show
(with none open, the first yet to end, or the last begun); `J` joins
the meeting open, or else, of those not cancelled or declined, the one
on or starting within 10 minutes (the one begun last). All-day events are on their (UTC) dates, which
are the same days wherever one is; timed events are shown in the
browser's time zone. `j`/`k` move by day, week or fortnight, `t` goes
to today, `d`, `w` and `a` switch views, `g c` goes to the calendar.

### 10.3 No write in MVP

Out of scope. Adding event creation is a v1.x story (ROADMAP). The `Op`
trait already accommodates write ops; we just don't implement them.

---

## 11. TODOs

### 11.1 Local, in the tx log

To-dos live in uplink's `todo` table (§4.3) and change only through ops
(`crates/uplink-todos`), so each change is a log entry with an inverse:
`todo.create` ↔ `todo.delete`, `todo.delete` ↔ `todo.put` (restoring the
row as it was, under its id), and `todo.edit`, which sets one field
(title, notes, due date, done). An edit's inverse carries the value the
edit made and is refused as a conflict if the field has changed since,
like a draft edit's (§7.2), so undo never clobbers a later change.

No `TodoBackend` trait: there is one store. Were a provider's tasks
(Microsoft To Do, say) added, they would sync into this table the way
mail syncs into `message`, with ops queueing the remote changes (§6.6);
an interface over "where to-dos are" would hide exactly that.

Requests: `ListTodos` (open ones first by due date, undated last, then
done ones, most recently done first), `CreateTodo { title, notes, due,
message }` (titles are non-empty, due dates real dates, the message
must exist), `EditTodo { todo, edit }`, `DeleteTodo { todo }`.

### 11.2 Flag for follow-up

A message is flagged exactly while it has an open to-do linked to it:
flagging is `todo.create` with `message` set (titled with the subject),
unflagging marks that to-do done. Each is one op, so undoing a flag
removes the to-do and with it the flag, in one gesture, with no
composite op.

The flag is uplink's own: it is not mirrored to Gmail (Gmail has no
flag; a star already means something else) nor, later, to M365's
follow-up flag. Mirroring M365's would be a reconciled desired state
(§6.6) derived from the to-do, if wanted.

A to-do flagged from a message opens it at `/message/:message`, the
message by itself (its entry in each account that has it; the first is
shown), and back is back to where it was opened from.

---

## 12. Frontend

### 12.1 Stack

- **Preact 10** with `@preact/signals` for reactive state.
- **preact-iso** for routing.
- **Vite 7** dev server / bundler.
- **TypeScript** strict.
- **TipTap** for the compose WYSIWYG: its framework-agnostic core
  (`@tiptap/core`, `@tiptap/starter-kit`), driven from a Preact effect, so
  no `preact/compat` (§15.1).
- **tinykeys** for keyboard shortcuts (Gmail-style `j`/`k`/`gi`/`c`).
- **@tanstack/react-virtual** for virtualized message lists.

No state-management library — `@preact/signals` is the store. No router
beyond preact-iso. No CSS framework — hand-written CSS modules.

### 12.2 Vite config

`web/vite.config.ts`: the dev server (port 5173) proxies `/api` and `/ws`
to the server on `127.0.0.1:8443`, as `uplink init` configures it (plain
HTTP). Its page is not under uplink's basic
auth, so the proxy adds the credentials from `UPLINK_DEV_AUTH=user:password`.
`vite build` writes to `crates/uplink-server/static/`, which the binary
embeds; in production the SPA's WebSocket connects to the same origin.
Vitest runs from the same config (jsdom).

### 12.3 Connection manager

`web/src/connection.ts` — single class wrapping the WebSocket with:

- Exponential reconnect (250 ms, 500, 1 s, 2 s, 4 s, 8 s, capped at 30 s).
- `visibilitychange` listener that triggers immediate heartbeat on
  visible-after-hidden.
- Outbound message queue, drained on connect.
- Cursor tracking: every received `Entry` updates the local cursor to its
  id; on reconnect, `Hello { since: cursor }` is sent.

### 12.4 Views

```
src/views/
├── App.tsx                # shell: header, nav, routes, notice bar
├── Nav.tsx                # drafts/outbox, mail labels, Matrix rooms
├── Home.tsx               # the agenda and the buckets (§12.13)
├── Mail.tsx               # conversation list and conversation view
├── Found.tsx              # a search's rooms, messages, to-dos, events
├── HtmlBody.tsx           # sandboxed HTML mail (§8.9)
├── Icon.tsx               # the Lucide icons used, vendored (ISC)
├── LabelPicker.tsx
├── Compose.tsx            # TipTap-backed, lazily loaded
├── Outgoing.tsx           # drafts and the outbox (§6.6)
├── Room.tsx               # a Matrix room: its timeline (§12.11)
├── RoomEvent.tsx          # an event of the timeline, on a line
├── RoomComposer.tsx       # the box to write to a room
├── Security.tsx           # a Matrix account's device and keys (§12.12)
├── Todos.tsx              # the to-do list
├── History.tsx            # the user's ops, with Undo
├── Calendar.tsx           # week and agenda (§10.2)
├── SearchBox.tsx
├── NoticeBar.tsx          # the last op, with Undo
└── Help.tsx               # keyboard shortcuts
```

The window's title names what is on show, as Gmail's and Element's do:
the most particular first, then `uplink` (`src/title.ts`). A list is
named with its unread, as Gmail's is, and its account
(`Inbox (3) – alice@gmail.com – uplink`), the Inboxes as one by theirs
(`All inboxes (5)`), a search by its query (`from:bob – Search`); a
conversation by its subject, a room by its name, a draft by its subject
(`… – Draft`); other views by their names in the nav. Until what it
names is loaded, the title is `uplink`.

### 12.5 Keyboard shortcuts

Gmail's scheme. The notable choices we ARE picking:

- `j`/`k` next/prev message (in a message: open the next/prev one).
- `o`/`Enter` open, `x` select, `*a`/`*n` select all / none, `*r`/`*u`
  read / unread, `*s`/`*t` starred / unstarred.
- `[`/`]` archive the open message and open the newer / older one.
- `gi`/`gs`/`gd`/`gt`/`ga` jump to Inbox / Starred / Drafts / Sent / All;
  `gl` to a label, from a filterable picker.
- `Shift+i`/`Shift+u` mark read / unread, `l` label (a filterable picker
  toggling a user label on the selection), `v` move to (a picker of user
  labels and the Inbox: adds the one chosen, and removes the label on
  show if it is a user label or the Inbox, else the Inbox), `+`/`=` and
  `-` mark important / not important.
- `c` compose, `r` reply, `a` reply-all, `f` forward.
- `e` archive, `#` delete, `!` mark as spam, `s` star.
- `/` focus search.
- `?` show shortcut overlay.
- `u` back to list.

The notable choices we are NOT picking:

Implemented: `j k o Enter u x *a *n e s l Shift+i Shift+u gi gs gt gd /
?` (M4), `c r a f` (M6), and in compose `Ctrl+Enter` send and `Esc` close
(the draft stays; these work from within its fields). They work with
the focus in a mail's frame too (on a link clicked), which has no scripts
to pass its keys on: the app listens on the frame's window, of its
origin, and Escape leaves the frame. `gd` goes to
uplink's own drafts; Gmail's DRAFT label is listed as "Gmail drafts".
`ga` goes to All Mail. `#` moves to
Trash and `!` reports spam, or not spam (§8.1; M10). Opening a message marks it read, as Gmail does, without
an undo notice. In a Matrix room, `l` tags it (§9.5), from a picker of
Element's two tags, the user's own of the account's rooms and the room's
others, toggling one; a name typed that no tag has is offered as a new
tag of the user's. In a Matrix room (M7), the message box has the focus, as a chat
app's does, so that keys are text: opening the room (by any means, the
switcher's too) or one of its threads focuses its box, and closing the
thread focuses the room's again; but not where the pointer is a finger
(`pointer: coarse`, a phone), where that would raise the on-screen
keyboard over what one came to read, and chat apps leave it to a tap. In the box `Enter` sends,
`Shift+Enter` breaks the line and `Esc` leaves it (once a reply or edit
is cancelled), for the shortcuts, as vim's normal mode; `i`, `Enter`
and `r` go back to it. Out of the box, `j`/`k` move to a line (from none, to the latest; in a
thread's list, within it), as the mail list's, and keys act on its
message, as Slack's do on the one it focuses: `r` replies, `e` edits
one's own, `t` opens its thread or replies in one, `+` opens its
reactions (at the first: `←`/`→` move along them, `Enter` takes one,
other keys type one into their box; the focus leaving closes them, and
closed, the line has it back), `p` pins or unpins it and `Delete` (on macOS, the
key so labelled: Backspace, as Apple Mail's; elsewhere, Backspace is
left to the browser, as Back where it is) or `#` delete one's own, once asked, as
deleting (by the menu too) cannot be undone once the homeserver has it:
the dialog's Delete has the focus, so `Enter` deletes and `Esc` keeps
it; what its
buttons do, but by keys. `o`/`Enter` (of the main timeline) open its
details beside the timeline: who sent it and when, its type, event id
and encryption, who has read up to it, its reactions, its edits and its
source; while open, they show the line that has the focus, so that
`j`/`k` (or a click) go on to the next, as in an open conversation. The line is the one that has the focus (its
outline shows only when keys put it there), so Tab goes on to its links
and buttons, where the keys still act on it; but Enter and Space, on a
link or button that has the focus, are its own: no window binding takes
them (so `Enter` writes only from no line). Its bindings count only
while the focus is in its list: a component's window bindings may be
scoped so, and of those of the same keys, one scoped where the focus is
fires, as nearer, before the latest component's.
`PageUp`/`PageDown` scroll the timeline a page
(less an eighth), from the box too, as Element's; from a thread's box,
the thread. The room's box unfocused is then the exception
(`Esc`, a click elsewhere on the page). Dialogs opened from the box
(edit history, a menu) close by `Esc` all the same, and a picker
closed gives the focus back to where it was, so that `Ctrl+K Esc` (or
`Ctrl+K` to the room on show) leaves one writing. A view shown has the
focus it takes, or none: going to another drops the focus of a field
outside it, as the search box. Gone back or forward to, a place has the
focus it had when left (a room's box, a line, or nothing), as browsers
give back where a page was scrolled: each history entry is stamped with
a key, and what has the focus kept by key, by its element's id
(`web/src/historyFocus.ts`), for the page's life. The `g` sequences also have
synonyms, `Alt+Shift` and the sequence's second key (`Alt+Shift+H`
home, `Alt+Shift+I` Inbox…), which work from within fields, as all
presses with Ctrl, Cmd or Alt held do: of modifiers, those that
browsers and systems take least. On macOS, where Option types
characters and browsers keep `Cmd+Shift` (Slack's `Cmd+Shift` go-tos
work only in its desktop app), they are `Ctrl+Option` and the key, as
Google Docs' menus, matched by the key's place (`KeyboardEvent.code`),
as Option changes what it types.
M10 added `*r *u *s *t [ ] v + = - gl`.
Above the list, Gmail's toolbar does the same by pointer: a box selecting
all or none (some selected, it selects none), with a menu of which to
select (`*` and a letter's), and, while some are selected, buttons to
archive, report spam (or not spam), delete, mark read or unread, flag,
move and label them, and a menu to star and mark important or not (as
each is not). With every conversation loaded selected and more to load,
it offers, as Gmail does, to select every one the list's query lists,
counted (`CountConversations`) when offered. Actions then apply to all
of them, as the server finds them when it does them (but flagging, which
adds a to-do each, is disabled), and leave
none selected. Above a conversation, a toolbar does the same to it, after
a button back to the list, and says where it is in the list (`3 of 50`,
or `of many` while more are to load), with buttons to the newer and the
older one (not on a phone, as Gmail's app has none).
`Shift+t` flags the targets for follow-up, or unflags them (M9; ⚑ marks
flagged messages); `gk` goes to the to-dos, where `j`/`k` move, `a`
focuses the box that adds one, `e` marks done or not, `#` deletes, and
`o`/`Enter` opens a flagged to-do's message, or renames any other (as
double-clicking its title does).
`Ctrl+K` (`Cmd+K` on macOS; not Gmail's) opens the switcher, a
filterable picker of every view the nav links to: the views above the
accounts, each account's labels and All Mail, and the Matrix rooms (not
joined spaces, which show no view). Visiting one by any means records it
(in the browser's `localStorage`, like the nav's folds); the picker lists
the most recently visited first, then the rest in the nav's order, and
the one on show last, so that `Ctrl+K Enter` goes back to the one
before. The filter matches every word typed, in the name or the
account's address. It works from within fields too: window bindings
ignore what is typed into a field, but not presses with Ctrl, Cmd or Alt
held.
In the pickers (`l`, `v`, `gl`, `Ctrl+K`), `Up`/`Down` move by one
option and `PageUp`/`PageDown` by a page (the options in view, less one,
as a native list does), `Enter` chooses and `Esc` closes.

- **`z` rewinds.** Anywhere, `z` undoes the latest op (an undo, it
  redoes). As each undo is itself an op, pressing `z` again would undo
  that undo; instead, the history store keeps where the last press got
  to, and the next goes on to the op before it, passing over what is
  undone, the undos (each of something older, passed over too), and
  what views did by being shown (§12.15: marking seen and read). It
  goes on only while the latest op is the one the last press left at the
  top: anything done since, in any tab, starts afresh from it. Presses
  are handled one after another, each from where the last got to. A
  failed undo is passed over too. Within the history (`g z`), `j`/`k`
  move and `z` undoes, or redoes, the focused entry instead.
- **No `;`/`:`, `m`, `b`, `.`:** uplink has no mute, snooze or "more
  actions" menu. No `n`/`p` (a conversation's next or previous message)
  yet.

- **The arrows move in lists as `j`/`k`**, as Slack's and Discord's do:
  `↓`/`↑` in the mail list, a room's timeline, home, the to-dos, the
  history and the outbox; not in an open conversation or the calendar,
  where they scroll.
- **`?` shows the shortcuts**: how keys work (what a box keeps, `Esc`,
  sequences), then those the view on show binds ("Here"), then the rest
  ("Elsewhere"), each with the platform's modifiers. Keys reach nothing
  behind it; `Esc` or `?` close it.
- **`Alt+↑`/`Alt+↓` move through the nav** (`Option` on macOS), from
  within fields too, as Element's and Slack's do through rooms: to the
  view linked before or after the one on show, in the nav as shown
  (what a fold hides is passed over), or from its end if none is;
  `Alt+Shift+↑`/`↓` to the nearest with unread (a count, or a bold
  name). They repeat while held.
- **`Esc` goes up**, pressed again and again, to a known place, first
  undoing what was done last: a field leaves itself (once it has
  cancelled what it was doing: a reply, a completion); a reply, an edit
  or a thread begun by a key on a line goes back to the line (begun by
  the pointer, the box keeps the focus), and the line replied to or
  edited (by `Up` too) shows, in view, while the box is at it; from a
  line, it closes a room's panel first (the details `Enter` opened, a
  thread), keeping one's place, then leaves the line, as anything else
  that has the focus (a link); from nothing, it closes a room's panel (a thread, the
  info, a search, a message's details), takes an open conversation back
  to its list, selects none, and else goes home. Dialogs, menus and
  pickers close by it first.

Sequences: all components' window bindings form one keymap with one
pending sequence (`web/src/keymap.ts`), on tinykeys' key parsing and
matching. A first press (`g`) thus keeps a binding of the second (`s`,
star) from firing too, and a sequence that just fired leaves nothing
half-matched behind (tinykeys' own handler does, so `*r` then `*u`
typed within a second lost the second). A sequence begun that waits half a second shows what may follow (a key
each, and what it does) at the foot of the window, as Emacs' which-key
does; shown, it waits for the press as long as it takes, rather than the
second it otherwise does, and Esc (or typing in a field) gives it up.
A held key repeats only `j` and
`k`: other actions, repeated, would act on more than meant (holding `e`
a moment too long would archive several messages).

### 12.6 Client state

`stores/client.ts` holds the tab's stores, which the connection feeds:
`ops.ts` (the ops dispatched from the tab, and the notice of the last one
with its Undo), `mail.ts` and `drafts.ts` (drafts and the outbox, plus
the compose sessions editing drafts in the tab, §7.2).

`stores/mail.ts` mirrors accounts and the list on show. Responses replace
what they cover; log entries apply on top — label changes and removals
locally (an entry losing the label on show leaves the list, focus moving
on), anything else (new entries, an entry joining the label) by reloading
the list's loaded range. This needs no id bookkeeping because the server
sends the entries a response reflects before the response (§5.4). A
query in flight when the socket drops is sent again on the next
connection; an op is failed, since it may have been applied (the log will
show it), except draft creation and edits, which can't apply twice
(§7.2). The last op dispatched from the tab is shown with an Undo button,
unless what it did shows at once where it was done, which is notice
enough: Matrix sends (the room shows them), draft creation and edits,
attaching to a draft, and read receipts. Failures are always shown.
Drafts apply their edits locally and reload on creation and deletion; the
outbox, while shown, reloads on `OutboxChanged`, and the nav's count of it
with it.

Lists are not virtualized yet: pages of 100 load as the focus or scroll
nears the end; a reload re-reads up to 500. `@tanstack/react-virtual`
comes in when lists get long enough to need it.

Tests: vitest + jsdom + `@testing-library/preact`; the connection against
`mock-socket`, the stores, compose sessions and views against an
in-memory stand-in for the server (`web/src/test/server.ts`).

### 12.7 Bundle budget

Target: < 250 KB gzipped JS for first paint. Achievable with Preact + the
listed libs (per the Matrix UI research, the Matrix-relevant external lib
set is ~155 KB gz; mail UI adds ~30 KB; routing/signals are tiny).

TipTap is heavy; we lazy-load `Compose.tsx` only when the user opens a
compose window.

---

### 12.8 Conversations

Actions apply to conversations (§4), from the list and from the
conversation view, to their entries in the list's scope, as Gmail's do:
archive, delete, spam, label, move, mark important and mark read to all
of them; mark unread to the latest. Star: the latest of each without a
star gets one; unstar: every star goes. The server does them
(`ActOnConversations`), so that an action on every match, of which the
client has loaded a page, is the same; which way a toggle goes is the
client's, as its button says: star if not each has one, else unstar;
report spam unless each entry is spam, else not spam. Each message
in the view has a star of its own. A row is unread if any entry is,
names its senders (`me` for the account's own address; by first names
if several), counts its messages, and shows the user's labels of any.
The view lists the messages oldest first, the unread ones and the
latest expanded (a header toggles one); opening it reads them all, and
replying, forwarding and flagging (`Shift+t`, or unflagging the flagged
ones) take the latest.

Not only by keyboard (for touch, too), as Gmail has them: below the
conversation, Reply to all, Reply and Forward, on the latest; on an
expanded message, by its date, Reply and a menu (⋮) of Reply, Reply to
all and Forward, on that message; Delete this message and Mark unread
from here (that message and those after it; the conversation is left,
as opening it would read them again); and Report spam, unless it is
spam; and Download message and Show original, the account's own copy as
stored (`GET /api/entries/{entry}/raw`: as `text/plain`, with `nosniff`
and `Content-Security-Policy: sandbox`, downloaded as `<subject>.eml`).
Deleting or reporting one message of several reopens the
conversation by a message that stays, since the path names one of its
entries; the last one leaves it. Escape, or a click elsewhere, closes
the menu.

A message's quote of the mail it answers is folded away, as Gmail's
"•••" does (`web/src/views/quote.ts`): from its first mark on, if only
quote follows it (a quote answered in between stays). A message of
nothing but quote, as one sent just for its attachment, is folded whole,
as Gmail folds it. Marks in HTML: Gmail's `.gmail_quote`, Yahoo's
`.yahoo_quoted`, any `<blockquote>` (with the attribution before it: an
element ending "wrote:", in a few languages, or Thunderbird's
`.moz-cite-prefix`), and Outlook's header of the original
(`#divRplyFwdMsg`, `#appendonsend`, or a top-ruled box starting
"From:", with the `<hr>` above it), from which all the rest goes. In
plain text: a trailing run of `>` lines with its attribution, or
Outlook's `-----Original Message-----` or underscore rule. Forwards
(subject `Fwd:` or `Fw:`) show whole. The HTML is folded in an inert
`<template>`, moving the quote into an element the frame's style hides;
the toggle, below the frame (which runs no scripts), reloads it with the
quote shown, and the frame is fitted to its root's box again.

### 12.9 Look

Gmail's, in light and dark (as the system is). Conversation rows: a
checkbox, a star that toggles Starred (§12.8), the senders and count,
the user's labels, the latest's subject and snippet, a paperclip if
files are attached to any (not inline images), and the date, small. The
rows share their columns (CSS subgrid), so the paperclip's and the
date's are only as wide as what is shown in them, and the subject and
snippet have the rest. Narrow, rows are laid out as a phone app's
(§16.1). Unread rows are bold on a background of
their own; read ones recede, their text dimmer (in the dark theme,
onto the page's near-black). The font is Gmail's (`Google Sans`, else
Roboto, Helvetica, Arial), narrower than most systems' own. No Important markers: Gmail's guesses, which `+` and `-`
still set. In the nav, system labels have Gmail's icons, the user's own
labels a tag, and Microsoft 365's folders a folder. Going anywhere, by any
means (a link, a shortcut, the switcher), brings the view's link in the
nav into view: the folds over it (its account's section, the labels or
spaces it is nested in) unfold, unless it is in view somewhere already
(a room in several spaces), and the nav scrolls to it; so too when the
narrow window's drawer opens.

### 12.10 Dates and times

One format throughout, `web/src/format.ts`: `YYYY-MM-DD` and 24-hour
`HH:MM`, in the browser's time zone (but queries' days are of UTC,
§8.7). Not the browser locale's
conventions: those come from the OS's regional settings, which Firefox
and glibc read differently (`LC_TIME=en_DK` gives ISO dates in a
terminal, but `10/11/2026, 11.51` in a page), and uplink has one user.
Message lists abbreviate, as Gmail's do: the time for today's mail, the
day and month (`9 Nov`) for this year's, and the date otherwise;
weekday names, in the calendar, are English. A preference can come
later.

### 12.11 Rooms

A room is laid out as Element's IRC layout lays it out: a line per
event, its time (`HH:MM`; the date on hover), its sender (a small avatar
and the name, in the colour Element gives that user id, cut to a column
of its own), and what it says. A line marks each day, and one ("New")
the fully-read marker (§12.15). It opens at its latest message. The header has the
room's avatar, name, topic, its tags (a button that opens `l`'s
picker), member count, whether it is encrypted, and the membership's
buttons.

- **Messages** show as last edited, marked `(edited)`; a click lists the
  edits (`ListEdits`). Their HTML is the server's sanitized
  `formatted_body` (§9.2), with its colours (`data-mx-color`), and
  spoilers revealed on click, code blocks with a button copying their
  text (on hover, at their bottom right, as the line's actions cover its
  top), as Element's; plain text has its URLs linked. Links to
  users and rooms (`matrix.to`, but those to events) show as pills, those
  of plain text named as the room's members and the account's rooms are:
  a click on a user's opens their profile, on a room's the room, if the
  account has it (else matrix.to). A room's address is resolved
  (`ResolveRoomAlias`) once shown, once per address: until it is, the
  pill says the address and opens matrix.to.
  Emotes show as IRC shows them (`* Name waves`), notices dimmed. A
  message the account's push rules highlight (as `RoomEvent::highlight`
  says: mentioning them, by `m.mentions`, their name or `@room`, or a
  keyword), but its own, is highlighted. Images and stickers show
  inline, at the size the event gives within 320×240 (stickers
  128×128), by their thumbnail; a click opens the full one. Video and
  audio play inline; other files are links that download them, with
  their size. A location links to OpenStreetMap. A reply shows what it
  answers above it, a click scrolling to it; a message of a thread
  whose root is not stored is marked so. A thread's root shows, under
  it, how many replies it has and the latest, a click opening the
  thread in a panel beside the timeline, as Element's: the root, the
  thread's messages and those being sent to it, and a box to write to
  it (text, with replies within the thread, edits and commands, as the
  room's). Any message not of a thread starts one, by its "Reply in
  thread" button. While a room shows, the search box at the top (which
  `/` focuses) searches it, not mail ("Search Lounge", without the
  query language's help link): its route (`/rooms/:room/search/:words`)
  shows a search of the room's messages (`SearchRoom`) in the panel,
  over any other, which closing it shows again: the messages found, newest first, by
  sender and time, the words marked, more on request; a click shows one
  in the timeline (loading earlier pages until it is, then scrolling to
  it and highlighting it), or one of a thread in its thread. One panel
  shows at a time: the room's info, a member's profile, a thread, or a
  search. Under the
  header, a room with pinned messages shows one at a time, as Element's
  banner, the latest pinned first ("Pinned message 1 of 2"): a click
  shows it in the timeline, as a search's, and the next; a message's menu
  pins and unpins it, if the room's power levels let the account. Deleted messages say so, with the reason; undecryptable
  ones too. Any other message type shows its `body`.
- **State events** are summarized after the sender's name ("Alice
  joined", "changed the topic to …"); three or more membership changes
  in a row fold into a line that unfolds. Events uplink does not show
  otherwise show their type and JSON. Any event's JSON shows from its
  menu ("View source").
- **Actions**, on hover, as Element's: react (Element's quick reactions,
  or any typed), reply, edit (the account's own messages of text), and a
  menu of delete (its own) and view source. Reactions show below the
  message, by key with their count, those of the account highlighted; a
  click adds one's own or takes it back.
- **Sends** that change a loaded event — reactions, edits, deletions —
  show on it while they go out (counted, "(saving…)", "Deleting…");
  others, and those needing a decision (refused, sent though
  cancelled), are listed after the timeline with their state and
  buttons (§9.3).
- **Gaps**, where events are missing (§9.2), are rows ("Earlier events
  are missing here") that fetch them as they come into view, or by
  their button; they fill in place, merged by position, and a gap left
  shows where they end, fetching on if it is in view.
- **Earlier pages** of the timeline load as its start ("Load earlier
  events") comes into view, page after page while it stays there. A
  load that brought something looks again; one that failed (the
  homeserver's refusal to fill a gap, say) is not repeated, but left to
  the row's button.
- **Starting a conversation**, from each Matrix account's "Start a
  conversation" in the navigation, as Element's "+": a dialog of three
  tabs, a new room (name, topic, users to invite, public with its
  address, encrypted — by default unless public, as Element), a direct
  chat with a user, or joining a room by address or id (§9.7). It says
  the server's refusal, and opens the room once listed.
- **Room info**, a panel beside the timeline that the member count in
  the header opens, as Element's right panel: the name and topic, a form
  if the account may change either; the room's address
  (`m.room.canonical_alias`, as synced), its id, and a `matrix.to` link
  to it (by its address, else by its id `?via=` the account's
  homeserver), each with a button to copy it; a box to invite a user by
  id; and
  the members (`ListMembers`) by membership (joined, invited, asking to
  join, banned; not those who left), each by power then name, with
  their role (Owner, a creator of a room of version 12; Admin, 100;
  Moderator, 50; else the level, if above 0). A click on one opens their
  profile in the panel, with a button back to the list.
- **A member's profile**, as Element's user info, opens in the panel
  from the room info's list or from a sender in the timeline (or a
  thread): their avatar, name, user id, role and membership (invited,
  asking to join, left, banned); "Message", which opens the account's
  direct chat with them, starting one if there is none (§9.7);
  "Mention", which puts a link mentioning them in the room's box where
  its caret is; and, as the account may, buttons to remove (revoke an
  invitation), ban (also one who left) or unban them, which ask a reason
  first. Each change is a send (§9.3), so undoable.
- **Who is typing** shows above the composer; who has read how far, as
  up to three avatars (and how many more) at the end of a line.
- **The composer** sends Markdown with Enter (Shift+Enter for a new
  line). A message beginning with `/` is a command, as Element's (`//`
  sends one `/`): `/me` (an emote), `/plain` (without Markdown),
  `/shrug`, `/tableflip`, `/unflip` and `/lenny` (plain text before the
  message), `/invite`, `/remove` (`/kick`), `/ban` and `/unban` (with a
  reason, but inviting), `/topic`, `/roomname`, `/join` (`/j`; opens the
  room) and `/leave` (`/part`). An unknown command, or one misused, says
  so above the box, which keeps the text. Completions show above the box
  as one types, as Element's: commands after a leading `/`, joined
  members (but the account) after `@`, by a word of their name or their
  id, inserted as a link that mentions them (then `@room`, from `@r`), and emoji after `:` and two
  characters, by their Slack-style shortcodes (Emojibase's data, loaded
  on first use); a whole `:shortcode:` typed becomes its emoji. Up and
  Down choose, Tab or Enter take one, Escape hides them until the text
  changes. Replying shows what it answers above the
  box; editing fills the box with the message, the draft kept apart.
  Up, in an empty box, edits the account's last message; Escape cancels
  a reply or edit. Files attached (the paperclip), pasted or dropped on
  the room are listed above the box, to remove or send with the
  message, as files after it (the first answering what it replies to if
  there is no text).

### 12.12 Matrix security

Each Matrix account lists a Security page under its header in the nav
(`/matrix/:account/security`): uplink's device, whether it is verified
(and if not, that others may not trust it and that it cannot read what
was sent before it), whether it has the cross-signing keys, the key
backup's and recovery's state; a form to recover with the recovery key
or passphrase, shown while the device is unverified or lacks the
secrets; room keys' import (a file and its passphrase, saying how many
keys were new) and export (a passphrase typed twice, downloaded as
`element-keys-<account>.txt`); and the account's devices, most
recently seen first, with when and where from, and whether each is
verified. It loads again on `SecurityChanged` notices and on reconnect.

While the device is unverified, it offers to verify with another
device, or with a device of the list. A verification shows what it
asks of the user, as Element does: waiting for the other device;
accepting or declining one asked of uplink; the seven emoji with their
names (and the numbers), to say whether they match the other device's;
then that it is done, or why it was cancelled. The nav counts, by the
account's Security item, verifications waiting for the user, so that a
request from another device is seen from any view: every Matrix
account's security is loaded for it.

### 12.13 Home

`/` is home: everything at a glance, of every kind; `g h` goes there
(and `g z` to the history, §6.3). `g i` still goes to the Inbox. Above, the agenda: the events of now through tomorrow, from
`ListEvents`, each a link to its day. Below, a row of the buckets'
names and counts, which scroll to them, and the buckets, configured in
`home` (§3.2), in order. `ListHome { limit }` lists them, in one
read: each bucket's query, less every earlier bucket's
(`(q₃) -(q₁) -(q₂)`), as `ListItems` lists it (§8.7), with up to `limit`
conversations and whether there are more; so an item is in the first
bucket that matches it. Unlike a search, a label or tag no kind has is
false rather than an error, since the default buckets name Gmail's
labels and Element's tags, which some accounts lack. A bucket's rows are
its to-dos (by due date), its events, then its mail and rooms, the most
recently active first (a room's row says, as mail's its subject and
snippet, what is new — `3 new`, `2 mentioning you` — and its latest
message, `Bob: Lunch?`, as last edited: `Room.preview`); it shows 20 at most (the client's `limit`), its
count saying `20+` when there may be more mail, with a link "Show all"
to the bucket's query as a search; its heading links there too, however
many it shows, to work through the bucket as a list. Mail opens within that search, so `j`
and `k` go through the bucket's mail; a room opens itself, a to-do its
message or the to-dos, an event its day. The keys act on any row: `j`/`k`
move the cursor, which stays on its row while home is left and comes
back (a row that goes gives it to the next one shown, or else the last), `o`/Enter open, `e` is done with it (archives mail, marks a room
read up to its latest event, `Room.latest`, or finishes a to-do), `s`
stars mail or favourites a room, and `l` labels mail or tags a room.
Rooms show what is unread, mentions, or an invitation; to-dos past due
show it. Home is listed while on show, again when a log entry may move
an item (mail's entries and labels, rooms, timelines, to-dos and
calendars changed), and when `HomeChanged` says the buckets were
configured anew; hidden, it is not kept current.

### 12.14 Badges and marks

A bucket of home (§3.2) with a `badge` shows on the app's icon, as
Gmail's, Element's and Discord's do, without their calling for the user
at whatever is unread: by default, Immediate's are counted, and Upkeep's
make a dot; Backlog and Leisure are left for the user to come to. A
count is of all a bucket holds, until its items leave it (read, done,
archived), as Slack's and Discord's counts of mentions stay until read;
a dot is of what a bucket holds that is unseen (§12.15), as what is
merely to be kept up with is news only until looked at. The icon counts
the items of the buckets badged `count`, up to `99+`, in red at its
bottom right; else, while a bucket badged `dot` holds anything unseen,
it has a dot; else it is the logo alone (`src/favicon.ts`, an SVG of
the logo with the mark over it). The window's title says what is on
show (§12.4), so it counts nothing of the rest.

Every place of the nav has a mark the same way, of the badged buckets'
items in it: a red count of those of `count` buckets, else a dot (in
the accent colour) while one of a `dot` bucket in it is unseen, else
nothing. Home's is the icon's. A place is where the nav links to an
item: a mail conversation is in each of its labels (Inbox and the
others listed, All Mail, and All inboxes for Inbox), a room in its own
link, a to-do in To-dos, an event in Calendar. A label counts its
conversations; a room counts its messages that notify, or 1 if none
does (an invitation, say). A folded space, label or account has the
strongest mark of what it folds: the sum of their counts, else a dot if
any has one.

Besides its mark, a place says what is unread, as it always has, more
quietly: a label its unread conversations, as a grey count (Gmail's);
a room, with unread messages, its name in bold, and no count (the
homeserver's notification counts, which Element shows, are the mark's
now). The place on show has its background only, not bold, so that it
is not taken for unread.

Counts look alike wherever they are (`Badge`): of what needs the user
(a mark's, mentions), a red pill; of what is unread, a grey pill (a
label's, a folded section's, All inboxes'); of how many there are (a
conversation's messages, Drafts, To-dos, a bucket's items), a plain
grey number. A row of mail, at home or in a list, has its messages'
count after its senders, as Gmail's, and how many are unread at the end
of its subject and snippet, apart (as Thunderbird's Total and Unread
columns, and Telegram's chats' pills at their right): unless it is of
one message, whose being unread its bold says. A room's row has, at the
end of its latest message, its mentions (`@2`, red), then how many are
unread.

The server derives and pushes them (`Badges`, §5.5), as home lists the
buckets (`ListHome`): of each badged bucket, its items, up to
`BADGE_LIMIT` (100), each by its latest activity (`Activity`: mail by
its conversation's latest entry, a room by its latest event, a to-do,
an event), so that what comes of an item anew (a reply, a message) is
another; and the marks of the places, of all its items, unlimited: in
one read per bucket, as the labels of the conversations it holds
(distinct, not each conversation), its rooms, and whether it holds any
to-do and any event, with, for its `dot`, those of what is unseen. It
derives them anew after log entries (as many as came while it did, at
once), when the buckets are configured anew, and each minute, as time
alone moves items (into `due:today`, out of `newer_than:`); it pushes
them only if they differ. The client sums folded places' marks itself,
as the folds are its own.

### 12.15 Seen and read

Two things are tracked of what comes in, as Thunderbird's "new" and
"unread", and Matrix's receipt and fully-read marker, are:

- **Seen**: the user has looked where it is. It is news until then,
  and the `dot` buckets' marks (§12.14) show only what is unseen.
- **Read**: the user has gone through it. Mail is read once opened, as
  before (`is:unread`, the provider's flag); a room's messages are read
  as they are shown on screen (below).

**Seen is of places.** Showing a place, in a visible tab (a label's or
All inboxes' list, a room, To-dos, Calendar, Home), sees everything in
it, however much is on screen: a new message in a label is seen once
the label is opened, though the message is not. Each place keeps when
it was last seen (desired state uplink alone keeps, §6.6; V0042
`seen`: of each kind, the latest id stored then, as ids are given in
the order things come; what was stored before is seen, as Home's, by
V0045); an item's activity is seen if any place it is
in (§12.14; Home holds whatever its buckets do) was
seen after it was recorded. So a conversation seen in the Inbox is
seen under its other labels too, and a reply to it is news again. The
client dispatches `MarkSeen { place }` on showing a place whose mark
has a dot, and on new activity in it while it shows; not otherwise, so
that the log is not filled with visits.

**Rooms open at their latest message**, as Element's and Discord's do,
for seeing what goes on now and for writing; catching up and mentions
have their own ways in. A room keeps two points, as Matrix does:

- **The read marker** (`read_up_to`, the `m.read` receipt, as before,
  §9.5) follows the latest message from others that has been wholly on
  screen, in a visible tab, as Element's receipt does: not the latest
  event, so that what was never shown stays unread. It only moves
  forward. Opening a room shows its latest messages, so it is read up to
  them, and its name is bold no more.
- **The fully-read marker** (`m.fully_read`, desired state as the read
  marker is, and pushed as room account data) is where catching up has
  got to: it moves forward over what has been on screen without a gap
  after it (as the timeline is scrolled down through, or up through and
  back), or, with the read marker, to the latest when the user is done
  (`Esc` while the bar below shows, "Mark as read" there, or `e` on
  home's room: `MarkRoomRead`'s `fully`). V0044
  `matrix_room.fully_read`, `fully_read_pushed`; `MarkFullyRead`,
  `MarkRoomFullyRead`. The line "New" marks it in the
  timeline (instead of where the unread began when the room was opened).

While the fully-read marker is above what is on screen, a bar at the
top says how many messages are after it, with "Jump to first unread"
(`Shift+PageUp`, as Element's) and "Mark as read". A mention of the user
(a message that highlights, §9.5, `RoomEvent.highlight`) after the
fully-read marker counts however the read marker moved: in
`is:mention`, so in Immediate's count, until catching up passes it (no
mark of its own: a mention seen out of order counts until what is
before it is caught up on, or the user is done). The bar says how many
of those loaded have not been on screen (`@ 2`) and jumps to the oldest
of them (`Shift+PageDown`), as Telegram's does.

**Showing is not doing.** What a view dispatches by being shown, not by
the user's hand — `MarkSeen`, `MarkRoomRead` and the fully-read
marker's moves as messages come on screen, and opening a conversation,
which reads it (`ActOnConversations`) — is logged and undoable as any
op, quietly (no notice, §12.6), but recorded as shown (V0043
`tx_log.shown`, `LogEntry.shown`; the op says, `Op::shown`, and the
requests that may be either carry `shown`): `z` passes over it (§12.5),
as if it were not there, so that seeing what an undo brings back does
not stop the rewinding; and the history (§6.3) lists it dimmed. Its
undo is the user's. `e` on home's room, or "Mark as read", is the
user's own.

## 13. Authentication & secrets

### 13.1 Browser auth

HTTP Basic Auth on every route (`/`, `/api/*`, `/ws`). The bcrypt hash
lives in `server.basic_auth.password_bcrypt`. The middleware uses
`subtle` crate for constant-time compare and `bcrypt` for verification.
bcrypt takes about 200 ms by design, and the browser sends the password
with every request: it is checked by bcrypt once (on a blocking thread),
then by its SHA-256 digest, remembered with the configured hash it was
checked against, so that changing the password voids it.

The browser sends the credentials with other sites' requests to uplink
too (Chromium unasked, Firefox once challenged), WebSocket handshakes
included, so those are refused (403) before auth: whatever `Sec-Fetch-Site`
says is not `same-origin` or `none` (the user's own), unless a top-level
`GET` navigation (a link to the app or a download); and a WebSocket whose
`Origin` is not at the `Host` asked for, as Chromium sends no
`Sec-Fetch-*` with one (a proxy in front must keep `Host`). Without
those headers, a request is no browser's, or one too old to send them.
The app's page has a CSP: only its own scripts, styles and images (and
`data:` images and inline styles, for mail, whose frame inherits it),
no `<object>`, `<base>` or form submissions, and no framing by other pages
(`frame-ancestors 'none'`).

This is intentionally minimal. The MVP doesn't need session cookies,
account selection, password reset, or rate limiting — the user is the
operator and `fail2ban` on the system handles brute-force concerns.

### 13.2 OAuth flow (Gmail)

- Prerequisite: the user creates their own Google Cloud project (Desktop
  app OAuth client) and copies `client_id` + `client_secret` into the
  YAML. See §15.2. The OAuth consent screen must be set to *In
  production* (unverified is fine for personal use): in *Testing*,
  Google expires refresh tokens after 7 days.
- One-time setup: `uplink oauth init --account=alice-gmail`. The CLI
  prints an authorization URL with PKCE and a loopback redirect
  (`http://127.0.0.1:<port>/`; Google no longer supports copy-paste
  "out-of-band" codes). If the browser is on another machine, the user
  pastes the URL it was redirected to back into the CLI. The CLI exchanges
  the code for refresh+access token, seals the refresh token (see §13.4),
  writes it to `accounts/alice-gmail.token`.
- Runtime: the account's sync task unseals the token with `master_key`
  on each access-token refresh (so a re-authorization takes effect
  without a restart), and re-seals it if Google rotates it. A refused
  refresh (`invalid_grant`) is retried with backoff like any failure
  until the account is authorized again.
- The token requests are a few form POSTs, made directly with `reqwest`
  (no `oauth2` crate). Google's endpoints are configurable per account
  (`endpoints: { auth, token, api }`, defaulting to Google's), so tests
  point them at a fake (§15.10).

### 13.3 OAuth flow (M365)

uplink signs in as a public client (no `client_secret`) of an Entra ID
app registration that the user, or their organization's administrator,
makes (README): as with Gmail (§15.2), uplink ships no client of its
own. The flow is **not** the same shape as Gmail's: it uses the v1
(classic) endpoint with a `resource=` parameter rather than v2 OIDC
with `scope=`, so the permissions are those configured on the
registration (Graph `Mail.ReadWrite`, `Mail.Send`, `Calendars.Read`)
rather than requested per sign-in:

- `oauth.client_id`: the registration's application id. Required.
- `oauth.authority`: default `https://login.microsoftonline.com/common`;
  a single-tenant registration names its tenant instead.
- Endpoints **v1**: `/oauth2/authorize`, `/oauth2/token` (NOT
  `/oauth2/v2.0/…`).
- `oauth.redirect_uri`: one the registration lists; default
  `https://login.microsoftonline.com/common/oauth2/nativeclient`, which
  Entra ID offers desktop applications. Nothing of uplink's listens
  there: the browser lands on a blank page whose URL carries the code.
  The CLI pairing step (`uplink oauth init --account <id>`, which serves
  both providers) says so and accepts the URL, a "…code=…" string, or
  the bare code, pasted on standard input.
- Authorize parameters: `response_type=code`,
  `redirect_uri=<redirect_uri>`, `response_mode=query`,
  `login_hint=<email>`, `resource=https://graph.microsoft.com` (or
  `https://outlook.office365.com` for the EWS resource when we need it).
  No PKCE: the v1 endpoint accepts the public-client form without.
  (Open: adding it, as defence in depth.)
- Token exchange parameters: `grant_type=authorization_code`, `code`,
  `redirect_uri`, `client_id`. No `scope`, no `resource`, no
  `code_verifier`.
- Cross-resource refresh: at the v1 endpoint, a refresh token issued
  against one resource can be redeemed for an access token against
  another by re-stating `resource=<target>` on the refresh POST. We
  do not currently rely on this — Graph is the only resource we hit —
  but the future EWS Streaming work (§8.3) will use it to mint
  `outlook.office365.com` tokens from the same sign-in.
- Refresh tokens may or may not rotate per call. Persistence atomically
  replaces the sealed token on every successful refresh (§15.4).

### 13.4 Sealing OAuth secrets at rest

- `master.key` is 32 random bytes (base64) in the data directory, made
  by whichever command first needs it (`init`, `oauth init`, `matrix
  pair`, `serve` with accounts): mode 0600, written to a temporary file
  and linked into place without replacing one made meanwhile. Its path
  is not configurable: the user found the options for it and the token
  files more to set up than they were worth.
- Token files contain `nonce || chacha20poly1305(master_key, nonce,
  refresh_token)`, with the account id as associated data, so a token
  file opens only for the account it was written for. Written to a
  temporary file (mode 0600) and renamed into place. Library:
  `chacha20poly1305` crate (RustCrypto).
- Why bother, given the trust boundary is server-disk: blast-radius
  reduction. A `cat` of a token file on a debugging session, a misplaced
  backup tarball, a btrfs snapshot copied to a less-secure machine — none
  of these alone yield a working OAuth token. The master.key is one extra
  file the attacker must also exfiltrate (a backup that leaves it out
  leaves the tokens sealed).
- Costs: ~50 LOC. Worth it.
- IMAP, POP3 and JMAP accounts' passwords are sealed the same way
  (`accounts/<id>.password`, by `uplink password --account <id>`,
  which reads it from the terminal or standard input): the server needs
  the password itself to sign in, so it cannot be hashed, and none is
  in the YAML.

### 13.5 Matrix credentials

The session (access token, device id) is sealed with the master key in
the account's directory, `matrix/<id>` under the data directory (§9.1,
§13.4). matrix-rust-sdk's stores there
are encrypted with a passphrase derived from the master key; the crypto
store holds the device's keys and, once paired with the recovery key,
the cross-signing keys. No password is stored anywhere: it is typed
into `uplink matrix pair` once.

### 13.6 TLS

- Optional. Without `server.tls`, uplink serves plain HTTP: the expected
  deployment is behind a reverse proxy that terminates TLS (Caddy), with
  uplink on loopback, which is the default `bind`. The SPA connects its
  WebSocket with `ws:` or `wss:` as the page was loaded.
- `rustls` (via `tokio-rustls`). Loads cert+key from
  `server.tls.cert`/`key` at startup.
- Hot-reload: file watcher fires on cert change, reloads PEM, swaps the
  `Arc<rustls::ServerConfig>` in the listener. New connections use the new
  cert; existing connections continue with the old.
- BYO cert. No ACME in MVP — user can use `certbot` or Caddy as a
  reverse-proxy for cert acquisition if needed.
- Mail servers (§8.10–8.13) are verified as browsers verify sites
  (`rustls-platform-verifier`: the system's roots).

### 13.7 OAuth flow (IMAP, POP3, SMTP and JMAP)

For servers that sign in by XOAUTH2 (or, JMAP's, by a `Bearer` token),
an account's `oauth` names the
provider's client and endpoints (`client_id`, optional `client_secret`,
`auth_url`, `token_url`, `scope`, which must ask for a refresh token,
e.g. `offline_access`). `uplink oauth init --account <id>` runs the
flow Gmail's does (§13.2: PKCE, a loopback redirect on any port, or the
redirected URL pasted), and the refresh token is sealed in
`accounts/<id>.token`; access tokens are refreshed as they expire, and
one the server refuses is refreshed for the next sign-in.

---

## 14. Milestones

Each milestone has an observable "Done when" criterion. The criterion is
what the implement agent should test against. The existing test suite
passing is necessary but not sufficient — new behavior gets new tests.

### M1. Skeleton

- Workspace crates created. `cargo check` and `cargo test` succeed.
- `flake.nix` builds the binary on NixOS.
- Empty axum server binds and serves a "hello" route on TLS.
- Vite SPA serves a "hello" page proxied through the dev server.
- CI: GitHub Actions or local nix flake check runs `cargo test` + `pnpm
  build` on each push.

**Done when:** `nix build` produces a working binary; visiting
`https://localhost:8443/` from a browser shows the placeholder page, behind
basic auth.

### M2. Configuration & inotify reload

- YAML parsing per §3.
- `notify` watcher.
- Section-diff broadcast.
- TLS cert hot reload.

**Done when:** modifying `uplink.yaml`'s `server.tls.cert` and
`accounts[]` triggers visible server-side log entries and the TLS reload
test passes (open a connection, swap cert, open a new connection, observe
the new SAN).

### M3. Storage + tx_log + Undo plumbing

- rusqlite + refinery migrations.
- `tx_log` table + `Op` trait + `dispatch` function.
- One sample op (e.g. `Sample { x: i64 }`) for testing.
- Undo endpoint + Redo (i.e. undo of undo) behavior.
- Broadcast hub (`tokio::sync::broadcast`).
- WebSocket protocol scaffold from §5.

**Done when:** dispatching `Sample`, getting a `tx_log` entry, undoing it,
and undoing the undo all round-trip correctly through SQLite and the
WebSocket. End-to-end Rust integration test that issues `Sample`, asserts
`tx_log` count, calls `Undo`, asserts the row is gone, asserts the WS
echoed both events with monotonic cursors.

### M4. Gmail receive

- Gmail API initial sync + `history.list` polling (§8.1).
- Message variants + .eml persist + FTS body extract.
- Frontend mail-list view with selection + keyboard nav.
- First real ops: mark read/unread, archive, label — with the Gmail
  reconciler pushing them via the API (§6.6).
- NixOS VM integration test against the Gmail emulator (§15.10).

**Done when**, in the VM test: a message delivered to the emulated Gmail
account appears in the UI's stream within one poll interval; archiving
it in uplink archives it in Gmail, and undoing the archive brings it
back; a label change made on the Gmail side shows up in uplink; FTS
search returns hits across subject/body/from; killing and restarting the
server loses no persisted messages and resumes from the stored
`historyId`.

### M5. M365 receive

- OAuth2 v1 auth-code flow as a public client (no PKCE, `resource=`
  parameter — per §13.3).
- Graph delta sync for mail, polling every 30 s per folder (no push in
  this milestone — see §8.3).
- Same .eml persist + FTS pipeline.

**Done when:** an M365 account, paired via the CLI, streams its messages
into the unified inbox alongside Gmail; the UI cleanly distinguishes
which account each message is from; the delta-link survives restart.

**State:** done against the emulator; not yet against a real tenant
(§15.12). `crates/uplink-email/src/m365/` syncs as §8.3 has it; its
tests (`tests/m365_sync.rs`) and the `integration-m365` VM test run
against emulate's Graph (§15.10): pairing with the real CLI and a pasted
code, first sync, new mail, archive and undo as moves, a move and a
category made in Outlook, an expired delta link, sending, and a restart
that records nothing. With more than one mail account, `/` is every
account's Inbox as one list (`All inboxes`), each row naming its
account.

### M6. Send + Compose + local Drafts

- TipTap compose UI.
- Local-only drafts as ops (§7).
- Outbox + send delay (§6.7); Gmail API send; Graph sendMail.

**Done when:** composing a reply, switching tabs mid-edit, and having the
draft survive a network drop all work; clicking send actually transmits;
undoing a send within the delay keeps it from going out; the provider's
copy of a sent message shows up within one sync interval as a second
variant of the same logical message.

**State:** done; Graph's two-step send against the emulator only
(§15.12).
"Switching tabs
mid-edit" is covered by the compose session tests (an edit from another
tab while typing, and while an edit is in flight), and by the e2e test,
which writes one draft in two Chromium tabs, both typing at once, as
well as replying and sending.

### M7. Matrix

- Account pair CLI subcommand (Secret Storage import).
- matrix-rust-sdk sync loop.
- Plain-text receive into `matrix_event`.
- Plain-text send through the outbox (§6.7).
- Room list + timeline UI.
- Unknown event types rendered as raw JSON.

**Done when:** sending a `m.text` to a room from uplink appears in
another Matrix client; sending one from the other client appears in
uplink; an unknown
event type (e.g. a sticker received from another client) appears in
uplink's timeline as raw JSON rather than crashing or vanishing.

**State:** done. `crates/uplink-matrix/tests/conduit.rs` runs each test
against its own Conduit, with another matrix-sdk client on the other
side: messages both ways in an encrypted room, an unknown event type
kept as raw JSON, read receipts both ways, resuming after a restart, and
pairing with the recovery key verifying the device. The `e2e` VM test
pairs an account with the real CLI and chats from Chromium.

### M8. Calendar (M365 read-only)

- Graph calendarView delta.
- Instances stored as Graph expands them (§10.1).
- Agenda view + week grid in SPA.

**Done when:** a recurring meeting on the M365 calendar appears in
uplink's day/week views at every occurrence over a 6-month range; a
modified single occurrence (exception) shows the modification, not the
master event.

**State:** done against the emulator (patch `0005`, §15.10), with no real
tenant to try it on yet (§15.12). The Rust tests (`uplink-calendar`'s
`tests/graph.rs`) list a weekly series over the window, with a moved and
a cancelled occurrence; follow changes through the delta link and move
the window as days pass; and list anew on a 410. `integration-m365`
checks the first sync of a series and a single event. The web tests show
a week and an agenda in New York, where local times and UTC dates
differ, and refresh on `calendar_changed`.

### M9. TODOs + flag-for-followup

- TodoBackend trait + local SQLite impl.
- Todo CRUD via dispatch ops.
- "Flag for follow-up" composite op (create todo, set message flag,
  single tx_log entry).
- Todo view in SPA.

**Done when:** flagging a message creates a todo with a link back to the
message; clicking the message link from the todo view opens the message;
undoing the flag removes both the todo and the message flag in one
gesture.

**State:** done, as §11 describes: no trait and no composite op, since
the flag is the open linked to-do. Unit tests cover ordering, undo of
each op (and refusal once changed since) and request checks; the web
tests flag, open and unflag from the keyboard; the `e2e` VM test flags a
message and adds a to-do in Chromium, and checks the rows.

### M10. Polish

- Cross-account FTS5 search UI.
- Transaction log view with Undo buttons.
- Keyboard shortcuts complete.
- Basic theming (light/dark).
- Documentation (README run-from-source guide).

**Done when:** the daily-use loop (read mail, reply, send a Matrix
message, check calendar, mark a todo done) feels usable without thinking;
search returns relevant cross-account hits in < 100 ms; the tx_log view
shows the last 100 user actions with working Undo buttons.

**State:** done (the daily loop's "check calendar" came with M8). Done:
the history view (§6.3);
light and dark themes following the system (`color-scheme`, so native
controls follow too; HTML mail keeps its white paper); `#` and `!` with
Gmail's hiding of Trash and Spam, and All Mail (`ga`) (§8.1); the rest
of Gmail's list shortcuts, with one keymap for sequences (§12.5);
joining and leaving Matrix rooms (§9.7); search and lists under 100 ms
on a 50,000-message mailbox (§8.7); attachments listed and downloaded,
and inline images shown (§8.9); attaching files when composing, and
forwarding them (§7.4); README status. Lists are still not virtualized
(§12.6): pages of 100 load as the focus nears the end, and a few
hundred rows render without trouble; it comes in if that changes.

### Why this ordering

- M3 (tx_log/undo) precedes any data ingest because it's the central
  plumbing every downstream op uses. Building Gmail first and retro-fitting
  undo creates rework.
- Gmail (M4) precedes M365 (M5) because the Gmail API is more straightforward
  than Graph's per-folder delta sync, so we hit fewer category errors at
  the start.
- Send (M6) sits before Matrix (M7) because compose is the most user-
  visible piece of the UI and pulls hard on the draft sync infrastructure,
  validating both before we add a second mode.

---

## 15. Risks & open items

These are decisions deferred or items needing user input.

### 15.1 TipTap under preact/compat

Stack research recommends Preact. TipTap is documented React-first; using
it via `preact/compat` is common but not officially supported. Risks:

- StrictMode incompatibilities (TipTap calls effects assuming single-pass).
- Subtle ref semantics differences breaking floating-menu portals.
- Bundle size from `preact/compat` shimming React APIs.

**Resolved (M6):** none of this applies, because TipTap's core is
framework-agnostic: `new Editor({ element })` in a Preact layout effect,
destroyed on unmount, with no React bindings and no `preact/compat`. It
works in Chromium (e2e test) and under jsdom (view tests). Its bundle
(~125 KB gzipped, mostly ProseMirror) is a separate chunk loaded when
compose first opens; the main bundle stays ~23 KB.

### 15.2 Gmail GCP project ownership

The Gmail API requires a Google Cloud project with OAuth client
credentials registered in Google Cloud Console. There is no
fully-decentralised path — OAuth requires a registered client. Two patterns are viable:

1. **BYO (MVP).** The user creates a one-off GCP project (~10 minutes in
   the Cloud Console), marks it as a Desktop/installed app, and pastes
   `client_id` + `client_secret` into uplink's YAML. Each install has its
   own OAuth project; the upstream uplink project has zero ongoing
   relationship with Google. This is what we ship in MVP. While the
   consent screen's publishing status is *Testing*, Google expires refresh
   tokens after 7 days; publishing it (no verification is required for a
   client only its owner uses) avoids re-running `uplink oauth init`
   weekly. README.md walks through the setup.
2. **Bundled (roadmap).** Register one GCP project under the uplink
   maintainer, ship `client_id` + `client_secret` with the binary (Google
   tolerates this for installed apps; the secret isn't truly secret). This
   becomes interesting only when uplink is shared beyond a single
   maintainer install, and requires Google's app-verification flow to
   raise the >100-user cap and remove the "unverified app" warning. Out
   of scope for MVP.

**Decision**: BYO-only for MVP. Document in README how to create the GCP
project. Bundled remains an additive future option — the same YAML
config schema covers both.

### 15.3 M365 app registration

The user brings the Entra ID app registration (§13.3), as they bring
Gmail's OAuth client (§15.2). Many organizations let users consent to
an application's delegated mail permissions; others reserve consent to
administrators, and there the user asks one to grant it. **Decision**:
BYO only; everything about the client (`client_id`, `redirect_uri`,
`authority`, `resource`) is configuration, and nothing but the OAuth
module depends on it.

### 15.4 Refresh-token rotation

Microsoft sometimes rotates refresh tokens on every refresh; sometimes
not. Our token-file persistence must atomically replace the token on
every refresh that returns a new one. **Decision**: write
to `tokens.<id>.<random>.tmp` and `rename(2)` over the canonical file.
Acknowledged design; nothing to spike.

### 15.5 SPA-frame auth: basic auth on the WS upgrade

Per stack research §8 risk 10, basic auth on the WS upgrade works but is
brittle across browser reloads. Some browsers prompt the user again,
some cache. **Decision for MVP**: accept the rough edge. Document
"reload requires re-typing credentials in some browsers". V1.x can add
session cookies.

### 15.6 Matrix sliding-sync vs v2 sync

**Resolved (M7): v2 `/sync`.** Every homeserver has it, and Conduit, the
test homeserver, advertises only the simplified sliding sync that
matrix-sdk's sliding sync is moving away from. v2 sync with a filter
(lazy-loaded members, no presence, receipts only) and uplink's own token
(§9.1) is simple and enough for a single user's rooms. Revisit if
initial sync of a large account proves too slow.

### 15.7 Items I didn't spike

The plan rests on these assumptions, which I did not prototype:

- **matrix-rust-sdk send local-echo with transaction IDs**: I assume the
  pattern of "insert pending row, swap on ACK, broadcast update" maps
  cleanly to the SDK's `send_transactional` or `send_with_txn_id` APIs.
  Worth a 1-hour spike at M7 start.
- **`mail-parser` performance on a 1 GB mailbox**: I assume that parsing
  ~1 M messages on first sync, on a modest CPU, completes in minutes
  not hours. Worth a benchmark if it ever feels slow.
- **`refinery` checksum-vs-migration-set behavior**: I assume `refinery`
  refuses startup if a previously applied migration's body has been
  edited. Worth a 30-min check.
- **Graph delta sync stale-token recovery**: when the delta token
  expires (Microsoft holds them ~7 days minimum, often much longer),
  Graph answers 410 Gone, and uplink lists that folder anew (§8.3),
  checking entries it last saw there but did not list. Tested against
  the emulator, which answers 410 for a token it does not know.

### 15.8 Storage capacity planning

Single user, btrfs, 50 GB target horizon. SQLite WAL on btrfs is fine
*provided* we set `nodatacow` on the DB directory (mandatory; CoW + WAL
fragmentation is pathological). `mail/` and `calendar/` should remain CoW
so reflink copies on snapshot don't double storage. Document this in
README install steps.

### 15.9 Irreversible external actions

**Decision** (revised 2026-11-10): sending is desired state like any other
(§6.6, §6.7), reconciled by the same task and shown in the same outbox
as label, membership and receipt changes. Undoing a send is never
refused: before the send is claimed it takes the mail back; afterwards,
if it went out, it is irreconcilable and the user keeps it sent. A retry
after an unknown outcome is made safe by observing the Message-ID in
Sent, or by sending a draft (Gmail, Graph), which sends once only, not by
refusing to retry. (Before: the outbox held sends only,
label changes were pushed unseen, and undo refused a sent mail.)
A Matrix send that went out is taken back by redacting it (§9.3), though
that is best-effort at most (recipients may already have the plaintext,
and the room shows a deletion): an undo the user asks for is done as
nearly as the provider allows, rather than left irreconcilable. (Before:
only a deletion of its own would redact it.)

### 15.10 Test infrastructure

CI runs exactly `nix flake check`. No separate workflow YAML to drift; the
flake is the source of truth.

**Flake checks (each runs independently and parallel-friendly):**

| Check                  | What it runs                                                  |
|------------------------|---------------------------------------------------------------|
| `backend-test`         | `cargo test --workspace`                                      |
| `backend-lint`         | `cargo clippy --workspace -- -D warnings` + `cargo fmt --check` |
| `protocol-ts`          | `web/src/generated/protocol.ts` matches `uplink-protocol`     |
| `frontend-test`        | `pnpm test` (vitest + jsdom + `@testing-library/preact`)      |
| `frontend-lint`        | `pnpm lint` (eslint: typescript-eslint, hooks rules)          |
| `frontend-build`       | `tsc` + `vite build` (the SPA the binary embeds)              |
| `integration-gmail`    | NixOS VM test: uplink service + Gmail emulator                |
| `integration-m365`     | NixOS VM test: uplink service + Graph fake                    |
| `e2e`                  | NixOS VM test: the web UI in Chromium (Playwright), keyboard-driven, against uplink + Gmail emulator + Conduit; screenshots in its output |

**Unit / property tests**

- Rust integration tests use a temporary `$DATA` directory + a fresh
  SQLite DB per test. Fixture helper provisions migrations, master key,
  and a stub `Config`.
- `rstest` for parameterized fixtures.
- Op-trait tests assert apply → inverse → apply round-trips for every
  Op variant.

**Integration tests (NixOS VM tests, `pkgs.testers.nixosTest`)**

Each runs the full stack as close to production as possible: uplink as a
systemd service with its real config, TLS and data directory, next to
fakes of the providers, driven over the WebSocket protocol by the test
script (e.g. inject a message on the provider side, watch it arrive,
archive it, check the provider's labels, undo). The service comes from
the flake's NixOS module (`nixosModules.default`, `nix/module.nix`), the
same one a deployment uses. The test script's WebSocket client
(`nix/tests/uplink_ws.py`) stands in for the UI; it uses Python
websockets' asyncio client, since the threaded one reads and writes one
TLS connection from two threads, which OpenSSL does not support (a TLS
1.3 session ticket arriving mid-write silently loses the upgrade
request).

- **Gmail:** [vercel-labs/emulate](https://github.com/vercel-labs/emulate)'s
  Google emulator (`packages.emulate`) — stateful Gmail REST plus Google
  OAuth. Spiked 2026-11-08: raw MIME import/get round-trips byte-exact;
  `modify`, `batchModify`, `send`, `labels`, `history` (all four change
  types) behave; the authorization-code flow (with PKCE) can be driven
  unattended by posting its sign-in form, so the test runs the real
  `uplink oauth init`; errors are Google-shaped.
  Gaps are fixed in emulate itself, never worked around in uplink: we
  build it from a pinned upstream commit plus `nix/emulate/patches/`,
  each patch ready to upstream (code, emulate's own tests, docs). The
  patches are exported from the `uplink` branch of a local clone and go
  upstream once uplink's tests have validated them. So far: `0001` adds
  `users.getProfile` (uplink's starting `historyId`) and a
  `gmail_history_limit` seed option under which `history.list` answers
  404 for a `startHistoryId` older than the retained history, so the
  full-resync fallback is testable; `0002` lets a loopback redirect URI
  match a registered one on any port, as Google does for installed apps
  (RFC 8252 §7.3), so `uplink oauth init` can listen on an ephemeral port;
  `0003` accepts multipart uploads (`multipart/related`: JSON metadata,
  then the message), as uplink sends mail (§8.2); `0007` sends as Gmail
  does, under a Message-ID and a Date of its own, a draft as a new
  message of its thread (the draft then gone), and takes a draft's
  multipart upload as its message.
  uplink's Google endpoints (auth, token, API base) are configurable,
  defaulting to Google's.
- **Real accounts.** A fake can diverge from the real service in
  minutiae (error handling especially), so the provider-facing tests are
  written once against endpoints passed in, and also run — manually, now
  and then — against a dedicated real test account on the host (needs
  network and credentials, so not part of `nix flake check`). Tests
  arrange provider-side state through the provider's own API (e.g. Gmail
  `messages.import` to deliver mail), which works in both modes, and
  clean up after themselves. Divergences found this way are fixed in the
  fake (upstream) or in uplink.
- **M365 (Graph):** emulate's Microsoft emulator covered OAuth only, so
  patch `0004` adds Graph mail to it: well-known and user folders
  (`mailFolders`, `childFolders`), per-folder `messages/delta` with
  next and delta links, `@removed` entries and 410 for an unknown delta
  token, messages created from MIME, `$value`, `PATCH`, `move` (ids
  immutable), `DELETE`, and `sendMail` (MIME or JSON) delivering to the
  seeded users; and the v1 `/{tenant}/oauth2/authorize` page, which
  uplink's flow uses (§13.3). Without seeded OAuth clients it accepts
  any client id and redirect URI. The Rust
  tests (`tests/m365_sync.rs`) and `integration-m365` run against it,
  arranging the Microsoft side through Graph as a real account's tests
  would; there is no real tenant to run them against yet (§15.12).
  Patch `0005` adds the calendar: `events` (create, get, `PATCH`,
  `DELETE`, of a series' occurrences too, which makes them exceptions or
  cancels them), `instances`, `calendarView` and `calendarView/delta`,
  expanding daily, weekly, absolute monthly and yearly patterns, in UTC
  only. Patch `0006` adds the two-step send (§8.4): drafts created from
  MIME (`POST /me/messages`), sent by id (`POST /me/messages/{id}/send`,
  refused for a message no longer a draft), and a folder's messages
  filtered by `internetMessageId`. Patch `0008` keeps a meeting's
  attendees, HTML body, `showAs` and online meeting (`isOnlineMeeting`
  gives it a `joinUrl`), and takes what Graph sets itself of someone
  else's meeting (`organizer`, `responseStatus`, `isCancelled`), so that
  tests and screenshots can seed one.
- **IMAP, POP3, SMTP and JMAP:** Stalwart (single-binary Rust mail
  server, from nixpkgs; IMAP, POP3, SMTP and JMAP), a real server rather
  than a fake. The Rust tests (`tests/imap_sync.rs`, `tests/pop3_sync.rs`,
  `tests/jmap_sync.rs`, the server in `tests/common/stalwart.rs`) start
  one per test
  (`UPLINK_STALWART`), its users in its in-memory directory and its TLS
  by a certificate the test makes and trusts, and play other mail
  clients over IMAP: delivering by `APPEND`, flagging, moving and
  expunging; mail uplink sends goes through its SMTP to the other
  user's Inbox; over JMAP, by `Email/import`, `Email/set` and
  `EmailSubmission`, its JMAP on an HTTPS listener whose URLs
  (`http.url`) name `localhost`. Its folders have special-use attributes
  but no Archive,
  which uplink then makes. XOAUTH2 is tested with emulate's Google as
  the OAuth provider: `uplink oauth init`'s flow runs on the account's
  own `oauth` settings, the browser played on emulate's sign-in form,
  and Stalwart, its directory then OpenID's (`oidc`), checks the access
  token uplink signs in with at emulate's userinfo endpoint, making the
  account as it first signs in. POP3 is reached by STLS, so both ways
  of TLS are tested; mail delivered by SMTP has a `Received` header, as
  on port 25 (which, by default, alone adds one).
- **Matrix:** Conduit (single-binary Rust homeserver, from nixpkgs), a
  real homeserver rather than a fake. The Rust tests start one per test
  (`UPLINK_CONDUIT`, set by the dev shell and the checks; in the build
  sandbox, through libredirect, since Conduit insists on reading
  `/etc/resolv.conf`), register users, and play the other side with a
  second matrix-sdk client. The `e2e` VM runs Conduit's NixOS service.
  No separate `integration-matrix` VM test: the Rust tests cover the
  sync task against the real thing, and `e2e` the deployment.

**Frontend tests**

- Component and store tests are vitest + jsdom with
  `@testing-library/preact`, against an in-memory fake of the server
  (`web/src/test/server.ts`) that, like the real one, delivers an op's
  log entry before its response. Tests find elements by role and
  accessible name, as a user would, not by component structure.
- The `e2e` check drives the real UI in Chromium (Playwright, browsers
  from `playwright-driver.browsers`) against the real server in a VM,
  with the keyboard, and saves screenshots.

### 15.11 M365 push mechanism is a moving target

MVP M365 push is polling the Graph delta link every 30 s per folder
(§8.3). This is "good enough, not great" — real Outlook gets sub-second
push via MAPI HangingNotification. The eventual replacement for our
polling is EWS Streaming Notifications, which:

- works today, given `EWS.AccessAsUser.All` on the app registration,
- is a long-poll over HTTPS, so no public endpoint is required, but
- is on Microsoft's retirement list (last announced sunset October 2026,
  has slipped before).

Before implementing EWS Streaming we should also research what current
first-party clients (new Outlook for Windows, OWA, Outlook Mobile)
actually use today; Microsoft may have shipped a Graph-native streaming
mechanism that simply isn't documented yet. That research is bundled
with the EWS Streaming work — whichever option is cheaper at the time
becomes the implementation. The two non-options:

- Graph webhook subscriptions require a publicly reachable HTTPS endpoint
  on the uplink host; incompatible with the "self-hosted on your laptop"
  baseline. Off the table.
- Manual refresh UI. Off the table on principle — the user must never
  see a "refresh" button.

**Risk horizon**: when (not if) Microsoft retires EWS and has not shipped
a Graph-native streaming alternative we can use, M365 push reverts to
polling. The polling code-path stays in the codebase for exactly this
reason and is the always-available baseline.

---

### 15.12 Not yet verified against the real services

There is no Microsoft 365 account to test with yet, so the M365 side is
verified against the emulator only (§15.10). Open until it runs against
a real tenant: the sign-in (§13.3: the authorize URL's parameters, the
`nativeclient` redirect's page, whether the token endpoint wants
anything more, and that the registration's permissions suffice), the
immutable-id `Prefer` header
together with `odata.maxpagesize`, `$select` on delta queries, whether
Graph's real delta pages report a move as a removal from one folder and
an addition to another (as the emulator does) and flags, importance and
category changes as updates, a draft created from MIME keeping our
Message-ID and its immutable id when sent, what `send` answers for a
draft already sent or being sent, and the
throttling of polling every folder every 30 s; for the calendar, that
calendar view delta expands series and reports exceptions and cancelled
occurrences as the emulator does, how long its delta links last, and
that `outlook.timezone="UTC"` gives UTC times (all-day events
included). The provider tests are
written against endpoints passed in, so pointing them at a real account
is a configuration step, as it is for Gmail.

Gmail is verified against a real account only as far as Google let a
throwaway one sign in, which it did not (automated sign-in is refused);
`UPLINK_TEST_GMAIL_CONFIG` runs the Gmail tests against an account
authorized by hand. The user's own account showed (2026-11-11) that
Gmail replaces the Message-ID and Date of mail it sends, which emulate
now does too (§8.2); that a draft sent is a new message, the draft then
404 to sending again, is from others' reports against the real API,
emulated likewise, but not yet seen on a real account.

## 16. UI shell

This section pins the load-bearing UI decisions that the data model
depends on. Detailed view-by-view design happens during the relevant
implementation milestone; this section captures only what the protocol
and stores must know to support.

### 16.1 Layout

Gmail-style two-column shell, with the right column swapping content
based on selection:

```
┌─────────────────┬──────────────────────────────────────────┐
│ Nav tree        │ Content pane                             │
│                 │                                          │
│ ▾ Mail          │  (exactly one of:                        │
│   • Inbox       │    - home / dashboard                    │
│   • Sent        │    - thread list                         │
│   • Drafts      │    - thread view                         │
│   ▾ Labels      │    - compose                             │
│     • work      │    - matrix room timeline                │
│     • personal  │    - matrix space view                   │
│ ▾ Chat          │    - calendar view                       │
│   ▾ #space      │    - todo list view                      │
│     • #room     │  )                                       │
│   • DMs         │                                          │
│ ▾ Calendar      │                                          │
│ ▾ Tasks         │                                          │
└─────────────────┴──────────────────────────────────────────┘
```

Unified nav tree: mail folders/labels, Matrix spaces+rooms (using Matrix
space hierarchy directly), calendars, and the tasks root all live in one
tree. Each top-level subtree is collapsible.

As built: Drafts, Outbox, History, To-dos and Calendar at the top; then
a subtree per mail account and per Matrix account, each folded by its
header (folded, it counts what is unread in it: the Inbox's
conversations, the rooms' messages). An account's labels nest by `/`,
as Gmail's and Microsoft 365's folder paths do, under parents that need
not be labels themselves, each foldable. What is folded is kept in the
browser (`localStorage`): it is the view's, not the mailbox's. A Matrix
account's rooms nest in its spaces (§9.5), each foldable; folded, a
space counts what is unread in its rooms.

Narrow (under 720 px wide, as a phone), the nav is a drawer, opened from
a menu button in the header and closed by going anywhere; message rows
are a phone app's: the sender and date, the subject, the snippet, and
the labels, with the star and paperclip at the right.

### 16.2 Route taxonomy

Routes drive which content view is active. The nav tree is always
visible. Routes (handled by `preact-iso`):

| Route                                  | Content pane                           |
|----------------------------------------|----------------------------------------|
| `/`                                    | Home / dashboard                       |
| `/mail/:account/:label`                | Conversation list (of a label, by provider id, e.g. `INBOX`) |
| `/mail/:account/:label/:entry`         | The entry's conversation               |
| `/all/:account`, `/all/:account/:entry` | All Mail, and a conversation in it     |
| `/search/:terms`                       | Search results                         |
| `/search/:terms/:entry`                | A conversation, within the results     |
| `/help/search`                         | The query language, with examples (§8.7) |
| `/message/:message`                    | A message's conversation (from a to-do) |
| `/compose/:draft`                      | Compose (new or continued draft)       |
| `/drafts`, `/outbox`                   | Drafts; what is not yet at the providers (§6.6) |
| `/rooms/:room`                         | Room timeline (uplink's room id)       |
| `/rooms/:room/search/:words`           | The timeline, and its messages searched (§12.11) |
| `/matrix/:account/space/:space`        | Space overview (deferred)              |
| `/calendar/:view/:date`                | Calendar, `week` or `agenda` (§10.2)   |
| `/todos`                               | Todo list                              |
| `/history`                             | The user's ops, with Undo (§6.3)       |

### 16.3 Nav tree as a WS query

The nav tree is a single subscription on the WS protocol, distinct from
content-pane subscriptions. Nothing folder-specific in the wire shape —
the tree is just one query result, the active content pane is another.
The server pushes `NavTreeDelta` events when accounts come online,
folders change, Matrix spaces are joined/left, etc. No specialness in
the protocol for the nav tree.

### 16.4 Keyboard shortcuts

Hardcoded Gmail-derived bindings for MVP. No user-configurable keymap;
preset support is deferred to roadmap. The bindings are the same on
every platform but for their modifiers: Cmd for Ctrl on macOS
(tinykeys' `$mod`), and `Ctrl+Option` for `Alt+Shift` (§12.5); the help
shows those of the platform. The source of truth is `web/src/keymap.ts`. `tinykeys` is the
implementation library.

### 16.5 Why nav tree + content pane is the right cut for this protocol

The two-pane split (rather than three-pane Gmail with separate thread-
list and reading-pane columns) keeps the WS protocol simple: at any
moment a client has exactly two active queries (nav tree + content), and
the server's delta semantics need to deal with only those two. Thread-
list ↔ thread-view transitions are route changes that swap the content
subscription. A future power-user "open thread in side panel" feature
would add a third subscription — out of scope for MVP.

---

## Appendix A — Crate dependency summary

| Crate                     | Use                                                        |
|---------------------------|------------------------------------------------------------|
| `axum` 0.7                | HTTP + WS server                                           |
| `tokio` (full)            | Runtime                                                    |
| `tower`/`tower-http`      | Middleware (basic auth, tracing, static files)             |
| `rustls` + `tokio-rustls` | TLS, with `Arc<ServerConfig>` hot-swap                     |
| `rusqlite`                | SQLite                                                     |
| `refinery`                | Migrations                                                 |
| `serde`/`serde_json`      | Serde                                                      |
| `ts-rs`                   | Rust → TypeScript type codegen                             |
| `mail-parser`             | RFC 5322 + MIME parsing                                    |
| `mail-builder`            | Outbound MIME assembly                                     |
| `ammonia`                 | Server-side HTML sanitizer                                 |
| `reqwest`                 | HTTPS, including the OAuth token requests                  |
| `clap`                    | CLI (`init`, `serve`, `oauth init`, `matrix pair`, …)      |
| `matrix-sdk`              | Matrix client with E2EE                                    |
| `notify`                  | inotify config reload                                      |
| `chacha20poly1305`        | OAuth token sealing                                        |
| `bcrypt`                  | Basic-auth password verification                           |
| `subtle`                  | Constant-time comparison                                   |
| `ulid`                    | ULID for draft ids                                         |
| `tracing`+`tracing-subscriber` | Structured logging                                    |
| `anyhow`/`thiserror`      | Errors                                                     |
| `rstest`                  | Parameterized tests                                        |

| Web pkg                   | Use                                                        |
|---------------------------|------------------------------------------------------------|
| `preact` + `@preact/signals` | UI + state                                              |
| `preact-iso`              | Router                                                     |
| `@preact/preset-vite`     | Vite plugin                                                |
| `typescript`              | TS                                                         |
| `tinykeys`                | Keyboard shortcuts                                         |
| `@tanstack/react-virtual` | List virtualization                                        |
| `@tiptap/core` + `@tiptap/starter-kit` | WYSIWYG compose (lazy-loaded)             |
| `vitest` + `@testing-library/preact` | Tests                                           |

---

## Appendix B — Code review checklist

For every PR:

- [ ] New mutating code paths go through the `tx_log` dispatcher.
- [ ] New ops have an inverse, an `apply` that is atomic, and a `describe`.
- [ ] Mass ops produce *one* tx_log entry, not N.
- [ ] No `.eml` file is mutated or deleted by the application.
- [ ] No defensive `if account is None` / `if message is None` branches
  protecting against impossible states. Fail-fast at the boundary instead.
- [ ] No new state stores. If you're tempted, ask why.
- [ ] WebSocket events carry a cursor.
- [ ] Wire-type changes are made in `uplink-protocol` and the generated
  `protocol.ts` is checked in.
- [ ] `cargo fmt`, `cargo clippy -- -D warnings`, `pnpm typecheck`.
- [ ] New behavior is testable; new tests assert observable outcomes,
  not implementation.

---

*End of document.*
