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
8. [Email pipeline (Gmail + M365)](#8-email-pipeline-gmail--m365)
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
                        │     │ + raw .eml/.ics on disk  │         │
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
4. **Raw `.eml` files and raw `.ics` blobs are sacred.** Content-addressed by
   SHA-256(raw bytes), never modified, never deleted by the application, no
   attachment extraction. Indexes (search, threading, labels) live in SQLite
   and are *derived* and rebuildable from the on-disk blobs.
5. **Real-time uses one channel: a JSON-over-WebSocket protocol uplink
   defines.** No SSE, no long-poll, no second port. Heartbeat is app-level
   ping/pong at 25–30 s. The stream is the log itself (§6): reconnection
   replays the entries after the last one the client saw.
6. **No client-side persistence other than IndexedDB write-through for the
   currently-being-edited draft.** No client cache of mail metadata, no
   client cache of Matrix events. The SPA loads from the server on connect,
   keeps reactive state in memory, and writes nothing back to disk except
   draft text resilience.

### 1.3 Out of scope for MVP (covered in `docs/ROADMAP.md`)

Quick references so the reader doesn't have to flip:

- All Matrix rich features (formatted body, reactions, replies, threads,
  edits, mentions, moderation, room settings). Plain text only; unknown
  event types render as raw JSON.
- E2EE on the client — server decrypts, client receives plaintext.
- SPA-side Matrix device-verification UI. Pairing is a CLI subcommand.
- SMTP send. Gmail sends via Gmail API; M365 sends via Graph.
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
│   ├── uplink-email/                # Gmail API, M365 Graph mail; send
│   ├── uplink-matrix/               # matrix-rust-sdk wrapper
│   ├── uplink-calendar/             # M365 Graph calendar (read-only)
│   ├── uplink-todos/                # TodoBackend trait + local impl
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

```yaml
# uplink.yaml

# ── server ──────────────────────────────────────────────────────────────
server:
  bind: "0.0.0.0:8443"
  tls:
    cert: /etc/uplink/cert.pem    # PEM, full chain
    key:  /etc/uplink/key.pem     # PEM, encrypted is rejected
  basic_auth:
    user: alice
    # bcrypt hash with cost ≥10. uplink rejects plaintext.
    password_bcrypt: "$2b$12$xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"

# ── data location ───────────────────────────────────────────────────────
data:
  root: /var/lib/uplink          # contains db/, mail/, calendar/, matrix/

# ── master key ──────────────────────────────────────────────────────────
# 32-byte random key, base64. Used to seal OAuth refresh tokens at rest.
# Generate with: uplink keygen > master.key
master_key_file: /etc/uplink/master.key

# ── accounts ────────────────────────────────────────────────────────────
accounts:

  # — Gmail —
  - id: alice-gmail
    kind: gmail
    address: alice@gmail.com
    # OAuth2: user brings their own Google Cloud project (Desktop app
    # OAuth client). Paste client_id + client_secret here.
    # Required scope: https://www.googleapis.com/auth/gmail.modify
    # (read, label, send; no permanent delete)
    oauth:
      client_id:     "1234.apps.googleusercontent.com"
      client_secret: "GOCSPX-xxxxxxxxxx"
      # path where uplink stores the encrypted refresh token
      token_file:    /var/lib/uplink/accounts/alice-gmail.token

  # — Microsoft 365 (work) —
  - id: alice-m365
    kind: m365
    address: alice@example.com
    # Impersonate the Outlook desktop public client. No client_secret;
    # uses the v1 AAD endpoint with `resource=` (no PKCE, no scope=) —
    # see SPEC §13.3. Granted permissions are determined by Outlook
    # desktop's pre-authorisation, not requested per-call. The Outlook
    # client_id is the documented escape hatch on locked-down tenants.
    oauth:
      client_id: "d3590ed6-52b3-4102-aeff-aad2292ab01c"
      authority: "https://login.microsoftonline.com/common"
      resource: "https://graph.microsoft.com"
      token_file: /var/lib/uplink/accounts/alice-m365.token

  # — Matrix —
  - id: alice-matrix
    kind: matrix
    homeserver: "https://matrix.example.org"
    mxid: "@alice:example.org"
    # Device login: created once via `uplink matrix pair`, stored as
    # access_token + device_id in matrix-rust-sdk's own state store under
    # data.root/matrix/alice-matrix/. No password in YAML.
    state_dir: /var/lib/uplink/matrix/alice-matrix
```

### 3.3 Hot-reload semantics

| Section change | Action |
|---|---|
| `server.tls.cert` / `key` | Reload PEMs, swap `Arc<rustls::ServerConfig>`. No connection drops. |
| `server.basic_auth` | Update in-memory hash. New requests authenticate against new hash. |
| `accounts[].oauth.*` | Re-issue refresh token if file changed; restart that account's sync task. |
| `accounts[]` add/remove | Spawn / shut down sync task. Push `AccountListChanged` to clients. |

Account changes currently require a restart (logged and ignored, like
`server.bind`); applying them live is deferred to M10. A sync task
re-reads its token file on every refresh, so re-running `uplink oauth
init` needs no restart.
| `server.bind` | **Restart required.** Logged as warning, ignored at runtime. |

### 3.4 Why not env vars / multiple files

- The user is the operator. One file is one place to look.
- Hot-reload of a directory tree is more complex than watching one file.
- Env vars don't survive `systemctl reload` cleanly.
- A single file is easy to btrfs-snapshot alongside data.

---

## 4. Storage & schema

### 4.1 On-disk layout

```
$DATA/
├── db/
│   ├── uplink.sqlite                 # main DB
│   ├── uplink.sqlite-wal
│   └── uplink.sqlite-shm
├── mail/
│   └── <sha256>/<sha256>.eml         # content-addressed; 2-char shard dir
├── calendar/
│   └── <sha256>.ics                  # content-addressed
├── matrix/
│   └── <account_id>/                 # opaque matrix-rust-sdk state store
└── accounts/
    └── <account_id>.token            # sealed OAuth refresh token
```

The `.eml` and `.ics` files are **sacred** (invariant 4). They are written
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
  sync_state  TEXT                 -- per-kind JSON; NULL until the first
                                   -- full sync has completed
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
  snippet           TEXT NOT NULL           -- start of the body text
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
CREATE INDEX mail_entry_by_thread ON mail_entry(account_id, thread_id);

CREATE TABLE label (
  id           INTEGER PRIMARY KEY,
  account_id   TEXT NOT NULL REFERENCES account(id),
  external_id  TEXT NOT NULL,      -- Gmail label id ('INBOX', 'Label_1')
  name         TEXT NOT NULL,
  system       INTEGER NOT NULL,   -- provider-defined (0/1)
  UNIQUE (account_id, external_id)
);

-- Label membership of an entry, desired (set by ops) and observed (set by
-- sync); the account's reconciler pushes differences (§6.6). Read and
-- starred state are labels too (UNREAD, STARRED). Rows with both 0 are
-- deleted.
CREATE TABLE entry_label (
  entry     INTEGER NOT NULL REFERENCES mail_entry(id) ON DELETE CASCADE,
  label     INTEGER NOT NULL REFERENCES label(id) ON DELETE CASCADE,
  desired   INTEGER NOT NULL,
  observed  INTEGER NOT NULL,
  PRIMARY KEY (entry, label),
  CHECK (desired OR observed)
);
CREATE INDEX entry_label_by_label ON entry_label(label, entry) WHERE desired;
CREATE INDEX entry_label_pending ON entry_label(entry) WHERE desired != observed;

-- Full-text index, one row per logical message (rowid = message.id).
CREATE VIRTUAL TABLE message_fts USING fts5(
  subject, from_addr, from_name, to_addrs, body_text,
  tokenize = 'unicode61 remove_diacritics 2'
);

-- ── matrix events (storage of all sync'd Matrix events) ─────────────
CREATE TABLE matrix_room (
  id            TEXT PRIMARY KEY,           -- room_id
  account_id    TEXT NOT NULL REFERENCES account(id),
  name          TEXT,                       -- best-guess display name
  topic         TEXT,
  avatar_mxc    TEXT,
  unread_count  INTEGER NOT NULL DEFAULT 0,
  highlight_count INTEGER NOT NULL DEFAULT 0,
  last_event_ts INTEGER,                    -- ms since epoch
  last_seen_ts  INTEGER                     -- our read marker (ms)
);
CREATE TABLE matrix_event (
  id              TEXT PRIMARY KEY,         -- event_id (with $)
  room_id         TEXT NOT NULL REFERENCES matrix_room(id),
  sender          TEXT NOT NULL,            -- mxid
  ts              INTEGER NOT NULL,         -- ms since epoch
  type            TEXT NOT NULL,            -- e.g. 'm.room.message'
  msgtype         TEXT,                     -- 'm.text' / null
  body            TEXT,                     -- plain text for m.text
  raw_json        TEXT NOT NULL             -- the full event content
);
CREATE INDEX matrix_event_by_room_ts ON matrix_event(room_id, ts DESC);

CREATE VIRTUAL TABLE matrix_event_fts USING fts5(
  sender, body,
  content='matrix_event', content_rowid='rowid',
  tokenize='unicode61 remove_diacritics 2'
);
-- triggers omitted for brevity, same shape as message_fts.

-- ── calendar (M365 read-only) ────────────────────────────────────────
CREATE TABLE calendar_event (
  id              TEXT PRIMARY KEY,         -- sha256(ics raw bytes)
  account_id      TEXT NOT NULL REFERENCES account(id),
  uid             TEXT NOT NULL,            -- iCal UID
  external_id     TEXT,                     -- Graph eventId
  summary         TEXT,
  description     TEXT,
  location        TEXT,
  start_ts        INTEGER NOT NULL,         -- unix seconds (or floating)
  end_ts          INTEGER NOT NULL,
  start_tz        TEXT,                     -- IANA name
  end_tz          TEXT,
  rrule           TEXT,                     -- RFC 5545 RRULE; client expands
  raw_path        TEXT NOT NULL             -- calendar/<sha256>.ics
);
CREATE INDEX calendar_event_by_account_start ON calendar_event(account_id, start_ts);

-- ── TODOs (local-only MVP) ───────────────────────────────────────────
CREATE TABLE todo (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  title         TEXT NOT NULL,
  body          TEXT,
  due_ts        INTEGER,
  done_at       INTEGER,                    -- null if not done
  -- link back to source message, if created via flag-for-followup
  source_message_id TEXT REFERENCES message(id),
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);
CREATE INDEX todo_by_done_due ON todo(done_at, due_ts);

-- ── drafts (local-only MVP) ──────────────────────────────────────────
-- Edited only through draft.* ops (§7); history lives in tx_log.
CREATE TABLE draft (
  id             TEXT PRIMARY KEY,          -- ULID
  account_id     TEXT NOT NULL REFERENCES account(id),
  subject        TEXT NOT NULL DEFAULT '',
  to_addrs       TEXT NOT NULL DEFAULT '[]',-- JSON array of strings
  cc_addrs       TEXT NOT NULL DEFAULT '[]',
  bcc_addrs      TEXT NOT NULL DEFAULT '[]',
  body_text      TEXT NOT NULL DEFAULT '',
  body_html      TEXT NOT NULL DEFAULT '',
  in_reply_to    TEXT,                      -- Message-ID of original
  updated_at     INTEGER NOT NULL
);

-- ── outbox (§6.7) ────────────────────────────────────────────────────
CREATE TABLE outbox (
  id             INTEGER PRIMARY KEY,
  account_id     TEXT NOT NULL REFERENCES account(id),
  kind           TEXT NOT NULL,             -- 'mail' | 'matrix'
  payload        TEXT NOT NULL,             -- JSON: the draft / message
  not_before     INTEGER NOT NULL,          -- unix ms; send delay
  state          TEXT NOT NULL,             -- 'queued' | 'sending' | 'sent' | 'failed'
  error          TEXT
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
  is whatever the provider tells us (Gmail `threadId`, M365 `conversationId`).
  We don't synthesize threads when the provider doesn't give them — MVP
  scope.
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

### 5.2 Message shape

All messages use a tagged enum discriminator. In Rust:

```rust
// crates/uplink-protocol/src/lib.rs

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsClientMsg {
    Ping { seq: u64 },
    /// First message after connection: the last entry the client saw,
    /// if it wants to resume.
    Hello { since: Option<TxLogId> },
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
  and observed membership per entry and label), `LabelListChanged`.
  Events carry ids, not content, since they are stored in the log;
  clients query what they display.
- **Matrix**: `RoomAdded`, `RoomRemoved`, `RoomNameChanged`,
  `RoomUnreadChanged`, `MatrixEventAdded` (carries decrypted plain text +
  raw JSON for unknown types), `RoomReadMarkerChanged`.
- **Calendar**: `CalendarEventAdded`, `CalendarEventRemoved`,
  `CalendarEventChanged`.
- **TODO**: `TodoAdded`, `TodoChanged`, `TodoRemoved`.
- **Draft**: `DraftEdited { id, edit }` (§7).
- **Outbox**: `OutboxItemChanged { id, state }`.
- **Config**: `ConfigChanged { section }`.

`ClientRequest` variants (MVP):

- `ListAccounts` — mail accounts with their labels and counts.
- `ListEntries { label, after, limit }` — a page of a label, newest first.
- `Search { query, limit }` — FTS5 cross-account search.
- `LoadMessage { message }` — fetch full body (text; sanitized HTML at M6).
- `LoadRoomTimeline { room_id, before_event_id, limit }`.
- `Undo { tx_log_id }`.
- One request per op kind the client can dispatch, answered with the
  tx_log id: `ModifyLabels { entries, add, remove }` covers archive,
  label, mark read and star; later draft edits, send, ...

Query responses carry `as_of`, the last log entry they reflect; `Undone`
and `Dispatched` reflect the log up to their own entry. The session sends
every entry up to that position before the response, and only later ones
after it, so a client replaces its state with a response and applies every
entry that follows on top, without comparing ids. (Requests are handled
one at a time per session, and the hub publishes in commit order, so the
session only ever waits for entries already committed.)

---

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
inverse is itself just a desired-state change. Whether it can still be
fulfilled is decided by the op: un-sending is possible only while the
item is still in the outbox (§6.7).

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

### 6.6 Desired state and reconciliation

Ops record what the user wants; they do not call providers. State that
mirrors a provider (labels, flags) is stored twice: *desired* (set by
ops) and *observed* (set by sync). Each account runs a reconciler task
that, after every commit touching its state, pushes the differences —
Gmail `messages.batchModify`, Graph `PATCH`/`move` — and retries with backoff on
failure. The UI shows desired state immediately, so actions feel instant
and survive a provider outage.

When sync observes a remote change:

- if there is no pending local change for that field, desired follows
  observed (the remote change wins);
- if there is, the local change wins and is pushed. Surfacing such
  conflicts to the user is deferred, together with undo conflicts (§6.2).

### 6.7 Outbox: actions that can't be taken back

Sending mail or a Matrix message is a desired state that can only be
fulfilled once. `mail.send` / `matrix.send` move the message into the
`outbox`; the account's reconciler performs the send once `not_before`
has passed. The inverse takes the item back out (returning a mail to its
draft) and succeeds only while the item is still `queued`; afterwards
undo refuses ("already sent"). A configurable send delay therefore gives
a reliable undo-send window. Sends stay in `tx_log` like any other op.

The queued item is also the local echo: it appears in Sent / the room
timeline immediately, marked pending, and becomes a normal message once
the provider's copy is observed (§8.6).

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
/// Replace `delete` with `insert` at `offset` (in chars) of a text field.
/// Applies only if `delete` is what is at `offset` now; otherwise the
/// edit is stale and refused.
struct DraftEdit { id: DraftId, field: TextField, offset: u32, delete: String, insert: String }
/// Set a non-text field (to/cc/bcc, in_reply_to), expecting `old`.
struct DraftSet  { id: DraftId, field: DraftField, old: Value, new: Value }
```

The inverse of an edit swaps `delete` and `insert`; the precondition is
the conflict check for both concurrent tabs and undo (§6.2). Each entry
stores only the changed span, never a copy of the draft.

The client debounces typing (~500 ms) and sends one `DraftEdit` covering
the difference between the text it last saw from the server and its
current text (common prefix/suffix). The server applies it and
broadcasts `DraftEdited`. A tab applies a remote edit directly if it has
no unsent local changes; if it does, it rebases them: it adopts the new
server text and sends a fresh diff of its own text against it. The
editor's undo keeps operating on keystrokes; tx_log undo steps back one
saved edit at a time.

`body_html` (TipTap output) is edited the same way, as text.

### 7.3 History

Runs of consecutive edits to the same draft are shown collapsed as one row
in the log UI ("edited draft 'Re: lunch'"), expandable. Because undo is
appended rather than destructive, the history of a draft is never lost —
in the spirit of vim's and emacs' persistent undo.

### 7.4 IndexedDB write-through

Every debounced edit is written to IndexedDB before it is sent, and
removed once acknowledged. On reconnect or startup, pending edits are
re-sent; any that come back stale are rebased as above.

---

## 8. Email pipeline (Gmail + M365)

### 8.1 Gmail — receive

All Gmail access goes through the Gmail REST API (`reqwest`, no SDK). Its
model — one message with many labels, `threadId`, a `historyId` change
feed — matches uplink's (desired/observed labels, §6.6) far better than
IMAP's per-folder copies, and a REST fake is practical to test against
(§15.10).

- **Initial sync**: `messages.list` (all mail, newest first) for ids, then
  `messages.get?format=raw` for content, stored as variants (§4.4), most
  recent first. Each batch is one log entry. Record the profile's
  `historyId` *before* starting, so nothing that changes during the walk
  is missed.
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

### 8.2 Gmail — send

- **Transport**: Gmail API `users.messages.send`. Build RFC 5322 message
  in-memory, base64url-encode it, POST.
- **Why API not SMTP**: avoids "Sent" folder synchronization mismatch
  (Gmail SMTP doesn't append to Sent; the API does).
- **Library**: hand-rolled HTTPS+JSON via `reqwest`. No SDK; Gmail send is
  ~30 lines of code.
- **OAuth scope required**: `https://www.googleapis.com/auth/gmail.send`.

### 8.3 M365 — receive

- **Transport**: Microsoft Graph `GET /me/mailFolders/{folder}/messages/delta`
  per folder on first call, then `@odata.deltaLink` for subsequent calls.
  Library: `reqwest` (no Rust Graph SDK; Graph is JSON+OAuth, easy to call
  directly). Raw MIME for storage via `/messages/{id}/$value`.
- **IMAP fallback**: none. Outlook desktop's pre-authorised scopes on the
  `outlook.office365.com` resource do not include `IMAP.AccessAsUser.All`
  (confirmed by spike `docs/spikes/outlook-impersonation.md`, May 2026 —
  Outlook itself does not use IMAP, so impersonating it cannot grant
  IMAP). All M365 mail flows through Graph.
- **Push — interim**: poll the delta link every 30 s per folder. Cheap
  under Graph throttling for a single user. The user must never see UI
  for "manual refresh"; polling is silent.
- **Push — future**: real Outlook gets push via MAPI HangingNotification
  (a long-poll over HTTPS, no public endpoint required). The closest
  third-party-accessible equivalent on M365 today is EWS Streaming
  Notifications (same long-poll pattern; Outlook desktop's token includes
  `EWS.AccessAsUser.All`). EWS is being retired by Microsoft (last
  announced sunset October 2026, slipping). A future milestone will:
  (a) research what current first-party clients (new Outlook for Windows,
  OWA, Outlook Mobile) actually use today — possibly a Graph-native
  streaming mechanism that doesn't yet appear in public docs — and
  (b) implement whichever of EWS Streaming or its Graph successor is
  cheapest. Until then, polling is the only mechanism. See §15.11.
- **Threading**: use `conversationId` from each message.
- **Folders**: Graph exposes mailFolders; each folder becomes a `label` row.
  Categories (`categories: ["red", "blue"]`) become additional labels
  prefixed with `cat:`.
- **OAuth**: Outlook desktop client_id impersonation
  (`d3590ed6-52b3-4102-aeff-aad2292ab01c`). v1 endpoint, `resource=`
  parameter, no PKCE. See §13.3.

### 8.4 M365 — send

- **Transport**: `POST /me/sendMail` with the RFC 5322 message we built
  (base64 MIME body), so our Message-ID is kept and the provider's copy
  becomes a variant of the same logical message. Verify at M6 that Graph
  preserves the Message-ID.
- **OAuth scope required**: `Mail.Send`.

### 8.5 Compose

- **Plain text body**: stored as-is in `draft.body_text`.
- **WYSIWYG**: TipTap editor in the browser; output is HTML. Stored in
  `draft.body_html`. On send, the HTML is sanitized (DOMPurify on the
  client *and* a Rust HTML sanitizer server-side as defense-in-depth).
- **HTML sanitizer**: `ammonia` crate on the server. Allowlist matches the
  set of tags TipTap can output.
- **Outbound MIME assembly**: `mail-builder` crate (sibling of mail-parser
  from stalwartlabs). Build `multipart/alternative { text/plain; text/html }`.

### 8.6 Persistence of sent mail

When the reconciler sends an outbox item:

1. Persist the assembled RFC 5322 bytes as a `local` variant, creating
   the logical message (its Message-ID is the one we generated).
2. Mark the outbox item `sent` and broadcast.

Next sync pass, the provider's Sent copy is observed. It has the same
Message-ID but different bytes (the provider adds headers), so it is
stored as a second variant of the same message, and the message gains its
`mail_entry` for the account.

### 8.7 Body extraction & FTS

See §4.5. mail-parser accepts anything; a message it cannot make sense of
is still stored and listed, with empty fields. Search matches every word
of the query as a word prefix; the query is not interpreted as FTS5
syntax.

### 8.8 Error semantics

- **OAuth refresh fails** (revoked token, password changed): set
  `account.enabled = 0`, broadcast `AccountListChanged` with an error flag,
  surface in UI as "Reauthenticate <account>".
- **Provider requests fail** (network, 5xx): retry with backoff (1s, 2s,
  4s, 8s, 16s, 30s, 60s cap). Log warning at 5 m of continuous failure.
- **Gmail / Graph 429**: respect `Retry-After`. Backoff in the same units.

---

## 9. Matrix pipeline

### 9.1 SDK & shape

`matrix-rust-sdk` (the crate Element X uses), with E2EE enabled
server-side. Configured to use SQLite as its state store (under
`$DATA/matrix/<account_id>/`). Crypto store is also SQLite; matrix-rust-sdk
manages migration internally.

```rust
use matrix_sdk::{Client, config::SyncSettings};
use matrix_sdk::ruma::events::room::message::SyncRoomMessageEvent;
use matrix_sdk::room::Room;
use matrix_sdk::encryption::EncryptionInfo;

let client = Client::builder()
    .homeserver_url(&cfg.homeserver)
    .sqlite_store(&cfg.state_dir, None)
    .build()
    .await?;

client.add_event_handler(
    |ev: SyncRoomMessageEvent, room: Room, enc_info: Option<EncryptionInfo>| async move {
        persist_message(ev, room, enc_info).await;
    },
);

client.sync(SyncSettings::default()).await?;
```

The `Option<EncryptionInfo>` parameter tells us whether the message arrived
encrypted (and was decrypted) or plaintext. We store this distinction in
the event row for display purposes.

### 9.2 Persistence

Every Matrix event flows through `persist_message`:

1. Insert into `matrix_event` with:
   - `type` = event type (`m.room.message` etc.)
   - `msgtype` = `m.text` / null
   - `body` = decrypted plain text (for `m.text`) or null
   - `raw_json` = the full event content, including any unrecognized fields
2. Broadcast `MatrixEventAdded` carrying the typed payload.

**For unknown event types (anything not `m.room.message` with `m.text`):**
the row still gets created with `body = null` and `raw_json` containing the
full content. The frontend renders the raw JSON in the timeline. This is
the "fail open" invariant from the ROADMAP — don't drop, don't crash,
display verbatim.

### 9.3 Sending

```rust
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;

let content = RoomMessageEventContent::text_plain(body);
let resp = room.send(content).await?;
// resp.event_id is the canonical id
```

Send is a `matrix.send` op that queues an outbox item (§6.7), which is
also the local echo. The reconciler sends it with the outbox id as the
transaction id; when the server's echo of the event is observed, the
pending item is replaced by the real event. If sending fails, the item is
marked `failed` and surfaced in the UI.

### 9.4 Cross-signing pairing

A CLI subcommand: `uplink matrix pair --account=alice-matrix`. UX:

```
$ uplink matrix pair --account=alice-matrix

Pairing the alice-matrix account with an existing trusted Matrix device.
You will need:
  - Access to a Matrix device of yours that has cross-signing already
    bootstrapped (e.g. Element on your phone), AND
  - Your Secret Storage passphrase OR recovery key.

[1/5] Logging in to homeserver…
      → homeserver: https://matrix.example.org
      → user:       @alice:example.org

      Password: ********

[2/5] Created device 'uplink/2026-05-20'.
      device_id = UPLABCDEFGH
      access_token saved to /var/lib/uplink/matrix/alice-matrix/

[3/5] Detecting Secret Storage on your account…
      → default key:  RcU... (key id Vh3T7...)
      → OK, Secret Storage is set up.

[4/5] Importing cross-signing secrets.
      Enter Secret Storage passphrase (or recovery key):
      ********

      → SSSS unlocked. Importing master_signing, self_signing, user_signing
        keys.
      → Cross-signing identity loaded.
      → Signing this device's curve25519+ed25519 keys.
      → Done.

[5/5] Verification check:
      → Device UPLABCDEFGH is now signed by your own master key.
      → Other Element devices will show this device as verified.

  ✔ Paired successfully. Restart uplink (or wait for inotify-driven
    reload) for the running server to pick up the new device.
```

Internally this uses:

```rust
let ss = client.encryption().secret_storage();
let store = ss.open_secret_store(&passphrase).await?;
store.import_secrets().await?;
// matrix-rust-sdk now has the cross-signing private keys;
// it signs this device automatically.
```

The reason this is a CLI flow and not a SPA flow: cross-signing setup is
done once per device, ever. Wiring it into the SPA would require a
device-verification UI that's largely orthogonal to MVP scope. The CLI is
~150 lines and adequate.

### 9.5 Rooms list / unread counts

- `MatrixSync` updates `matrix_room.unread_count` and `highlight_count`
  from the SDK's sync response.
- Broadcast `RoomUnreadChanged` for each room whose counts changed.
- Read marker (`m.fully_read`): set via `room.send_receipt(...)` whenever
  the SPA reports a "viewed up to event X" event for that room.

### 9.6 What we deliberately don't do

- No formatted_body rendering. ROADMAP item, deferred.
- No reactions, edits, threads, replies — also deferred. These arrive in
  `matrix_event.raw_json` and render as raw JSON in the timeline.
- No SPA-side device management or verification UI.
- No moderation UI (kick/ban/redact). The CLI can do `uplink matrix
  redact <event>` if needed in an emergency; not implementing in MVP.

---

## 10. Calendar pipeline

### 10.1 M365 only, read-only

- **Transport**: Graph `GET /me/calendars/{cal}/calendarView/delta` with
  `startDateTime`/`endDateTime` parameters covering a rolling 1-year
  window (configurable). Persists `@odata.deltaLink` in
  `account.sync_state`.
- **Polling**: every 5 minutes.
- **Storage**: per Graph event, fetch the `.ics` representation
  (`?$select=...&prefer=outlook.body-content-type="text"`) and write it
  to `calendar/<sha256>.ics`. Insert `calendar_event` row.
- **Recurring events**: Graph returns expanded instances *or* master events
  with RRULE. We store the master with `rrule`, and the SPA expands them
  client-side using the `rrule` JS library (the same library Element uses
  for `m.room.encryption` rotation period parsing — not Matrix-related, but
  general-purpose JS rrule). For master events with exceptions, we also
  store each exception as its own row.

### 10.2 Display

Calendar view in the SPA: agenda (list) + day/week grid. Recurring rules
expanded client-side. Time zones honored per event.

### 10.3 No write in MVP

Out of scope. Adding event creation is a v1.x story (ROADMAP). The `Op`
trait already accommodates write ops; we just don't implement them.

---

## 11. TODOs

### 11.1 Trait

```rust
#[async_trait]
pub trait TodoBackend: Send + Sync {
    async fn list(&self) -> Result<Vec<TodoRow>>;
    async fn create(&self, t: TodoCreate) -> Result<TodoRow>;
    async fn update(&self, id: i64, patch: TodoPatch) -> Result<TodoRow>;
    async fn delete(&self, id: i64) -> Result<()>;
}
```

### 11.2 Local-SQLite impl

The only MVP implementation: writes to the `todo` table directly. CRUD
operations go through the dispatcher as `TodoCreate` / `TodoUpdate` /
`TodoDelete` ops with the obvious inverses.

### 11.3 Flag-for-followup integration

When the user invokes "flag this message for follow-up" on a `message`, the
op is a *composite*: `MailFlagAndCreateTodo { message_id, todo: TodoCreate }`.
Single tx_log entry. Inverse undoes both the flag and deletes the todo.

This composite-op pattern is the recommended way to handle any cross-domain
user action in MVP — don't fan out into multiple ops, keep undo atomic.

---

## 12. Frontend

### 12.1 Stack

- **Preact 10** with `@preact/signals` for reactive state.
- **preact-iso** for routing.
- **Vite 7** dev server / bundler.
- **TypeScript** strict.
- **TipTap** via `preact/compat` for the compose WYSIWYG.
- **DOMPurify** for any sanitization on the client (defense in depth).
- **tinykeys** for keyboard shortcuts (Gmail-style `j`/`k`/`gi`/`c`).
- **@tanstack/react-virtual** for virtualized message lists.
- **rrule** for client-side recurring-event expansion.

No state-management library — `@preact/signals` is the store. No router
beyond preact-iso. No CSS framework — hand-written CSS modules.

### 12.2 Vite config

`web/vite.config.ts`: the dev server (port 5173) proxies `/api` and `/ws`
to the TLS server on `127.0.0.1:8443`. Its page is not under uplink's basic
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
- IndexedDB write-through for draft text (per §7).

### 12.4 Views

```
src/views/
├── Mail.tsx               # 3-pane: account list | message list | message
├── MessageList.tsx
├── MessageView.tsx
├── Compose.tsx            # TipTap-backed
├── Matrix.tsx             # 2-pane: room list | timeline
├── RoomTimeline.tsx
├── Calendar.tsx           # agenda + month grid
├── Todos.tsx              # list + add
├── TxLog.tsx              # transaction log with Undo buttons
├── Search.tsx             # cross-account FTS results
└── Settings.tsx           # read-only display of YAML config sections
```

### 12.5 Keyboard shortcuts

Gmail's scheme. The notable choices we ARE picking:

- `j`/`k` next/prev message (in a message: open the next/prev one).
- `o`/`Enter` open, `x` select, `*a`/`*n` select all / none.
- `gi`/`gs`/`gd`/`gt`/`ga` jump to Inbox / Starred / Drafts / Sent / All.
- `Shift+i`/`Shift+u` mark read / unread, `l` label (a filterable picker
  toggling a user label on the selection).
- `c` compose, `r` reply, `a` reply-all, `f` forward.
- `e` archive, `#` delete, `!` mark as spam, `s` star.
- `/` focus search.
- `?` show shortcut overlay.
- `u` back to list.

The notable choices we are NOT picking:

Implemented at M4: `j k o Enter u x *a *n e s l Shift+i Shift+u gi gs gt
gd / ?`. `ga` waits for an All Mail view (lists are of a label), `#` and `!`
for trash/spam ops, `c r a f` for compose (M6). Opening a message marks it
read, as Gmail does, without an undo notice.

- **No `z` for undo.** Undo lives in the transaction log view and as an
  optional toast immediately after each mutating op. `z` overloads
  unhelpfully when search/compose has focus, and the tx_log is the
  better surface for the multi-step undo we actually support.

### 12.6 Client state (M4)

`stores/mail.ts` mirrors accounts and the list on show. Responses replace
what they cover; log entries apply on top — label changes and removals
locally (an entry losing the label on show leaves the list, focus moving
on), anything else (new entries, an entry joining the label) by reloading
the list's loaded range. This needs no id bookkeeping because the server
sends the entries a response reflects before the response (§5.4). A
query in flight when the socket drops is sent again on the next
connection; an op is failed, since it may have been applied (the log will
show it). The last op dispatched from the tab is shown with an Undo
button.

Lists are not virtualized yet: pages of 100 load as the focus or scroll
nears the end; a reload re-reads up to 500. `@tanstack/react-virtual`
comes in when lists get long enough to need it.

Tests: vitest + jsdom + `@testing-library/preact`; the connection against
`mock-socket`, the store and views against an in-memory stand-in for the
server's mail requests (`web/src/test/server.ts`).

### 12.7 Bundle budget

Target: < 250 KB gzipped JS for first paint. Achievable with Preact + the
listed libs (per the Matrix UI research, the Matrix-relevant external lib
set is ~155 KB gz; mail UI adds ~30 KB; routing/signals are tiny).

TipTap is heavy; we lazy-load `Compose.tsx` only when the user opens a
compose window.

---

## 13. Authentication & secrets

### 13.1 Browser auth

HTTP Basic Auth on every route (`/`, `/api/*`, `/ws`). The bcrypt hash
lives in `server.basic_auth.password_bcrypt`. The middleware uses
`subtle` crate for constant-time compare and `bcrypt` for verification.

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

### 13.3 OAuth flow (M365 with Outlook impersonation)

Confirmed by the spike `docs/spikes/outlook-impersonation.md` (May 2026)
against a real corporate tenant. The flow is **not** the same shape as
Gmail — it uses the v1 (classic) AAD endpoint with a `resource=` parameter
rather than v2 OIDC with `scope=`, and the Outlook desktop client_id
requires the URN-form redirect URI:

- `client_id = d3590ed6-52b3-4102-aeff-aad2292ab01c` (Outlook desktop),
  no `client_secret`.
- Authority `https://login.microsoftonline.com/common`.
- Endpoints **v1**: `/oauth2/authorize`, `/oauth2/token` (NOT
  `/oauth2/v2.0/…`).
- Redirect URI **`urn:ietf:wg:oauth:2.0:oob`**. The `nativeclient` URL
  works for DavMail's own client_id but is NOT registered against
  Outlook desktop's; using it returns `AADSTS50011`. The OOB URN
  redirect means AAD returns the code via a `Location: urn:...?code=...`
  header that the browser cannot follow — the user must extract the
  code from DevTools or from whatever results page AAD renders. The
  CLI pairing step explains this and accepts any of the observed forms
  (URN, full URL, "Success code=…" string, or bare code).
- Authorize parameters: `response_type=code`, `redirect_uri=<URN>`,
  `response_mode=query`, `login_hint=<email>`,
  `resource=https://graph.microsoft.com` (or
  `https://outlook.office365.com` for the EWS resource when we need it).
  No PKCE — DavMail doesn't use it for this flow and the v1 endpoint
  accepts the public-client form without.
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
- Granted scopes are determined by Outlook desktop's pre-authorisation,
  not requested per-call. The Graph token gets the full Outlook scope
  set: `Mail.ReadWrite Mail.Send Calendars.ReadWrite Contacts.ReadWrite
  Files.* People.* User.Read.*` and more. No `IMAP.AccessAsUser.All` —
  Outlook itself does not use IMAP. See §8.3.

### 13.4 Sealing OAuth secrets at rest

- `master.key` is a 32-byte file generated once by `uplink keygen` and
  stored at `master_key_file` (default `/etc/uplink/master.key`, mode 0400
  owned by the uplink user).
- Token files contain `nonce || chacha20poly1305(master_key, nonce,
  refresh_token)`, with the account id as associated data, so a token
  file opens only for the account it was written for. Written to a
  temporary file (mode 0600) and renamed into place. Library:
  `chacha20poly1305` crate (RustCrypto).
- Why bother, given the trust boundary is server-disk: blast-radius
  reduction. A `cat` of a token file on a debugging session, a misplaced
  backup tarball, a btrfs snapshot copied to a less-secure machine — none
  of these alone yield a working OAuth token. The master.key is one extra
  file the attacker must also exfiltrate, and on a well-configured system
  it can live on a different filesystem with stricter ACLs (e.g.
  `/run/secrets/`).
- Costs: ~50 LOC. Worth it.

### 13.5 Matrix credentials

`matrix-rust-sdk` stores its own access tokens in its sqlite_store under
`$DATA/matrix/<account_id>/`. We don't intermediate. Encryption-at-rest of
that store is whatever matrix-rust-sdk does (currently: the SQLite store
is in plaintext on disk, the crypto store has encrypted secrets). Good
enough for MVP — same trust boundary as everything else server-side.

### 13.6 TLS

- `rustls` (via `tokio-rustls`). Loads cert+key from
  `server.tls.cert`/`key` at startup.
- Hot-reload: file watcher fires on cert change, reloads PEM, swaps the
  `Arc<rustls::ServerConfig>` in the listener. New connections use the new
  cert; existing connections continue with the old.
- BYO cert. No ACME in MVP — user can use `certbot` or Caddy as a
  reverse-proxy for cert acquisition if needed.

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

- OAuth2 v1 auth-code flow with Outlook-client impersonation (no PKCE,
  `resource=` parameter, URN redirect — per §13.3).
- Graph delta sync for mail, polling every 30 s per folder (no push in
  this milestone — see §8.3).
- Same .eml persist + FTS pipeline.

**Done when:** an M365 account, paired via the CLI, streams its messages
into the unified inbox alongside Gmail; the UI cleanly distinguishes
which account each message is from; the delta-link survives restart.

### M6. Send + Compose + local Drafts

- TipTap compose UI.
- Local-only drafts as ops (§7).
- Outbox + send delay (§6.7); Gmail API send; Graph sendMail.

**Done when:** composing a reply, switching tabs mid-edit, and having the
draft survive a network drop all work; clicking send actually transmits;
undoing a send within the delay keeps it from going out; the provider's
copy of a sent message shows up within one sync interval as a second
variant of the same logical message.

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

### M8. Calendar (M365 read-only)

- Graph calendarView delta.
- .ics persist.
- Agenda view + month grid in SPA.
- Client-side RRULE expansion.

**Done when:** a recurring meeting on the M365 calendar appears in
uplink's day/week views at every occurrence over a 6-month range; a
modified single occurrence (exception) shows the modification, not the
master event.

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

### Why this ordering

- M3 (tx_log/undo) precedes any data ingest because it's the central
  plumbing every downstream op uses. Building Gmail first and retro-fitting
  undo creates rework.
- Gmail (M4) precedes M365 (M5) because the Gmail API is more straightforward
  than the OAuth-impersonation dance, so we hit fewer category errors at
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

**Decision pending**: do a 1-day spike *during M6* to confirm TipTap +
preact/compat works for our compose needs. Fallbacks (in order): Lexical
(framework-agnostic), ProseMirror direct (more LOC, no React dep).

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

### 15.3 M365 Outlook-client impersonation longevity

Per stack research §3, Microsoft could in principle clamp down on
non-Microsoft processes using the Outlook desktop client_id. Risk horizon:
1–3 years. **Mitigation**: keep the auth layer pluggable so we can swap
to a tenant-registered Entra app. IMAP+XOAUTH2 is **not** an available
fallback — Outlook desktop's pre-authorisation does not include
`IMAP.AccessAsUser.All` (confirmed by the M365 impersonation spike,
May 2026; Outlook itself does not use IMAP). Do not couple anything
but the OAuth crate to the client_id.

### 15.4 Refresh-token rotation under Outlook impersonation

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

matrix-rust-sdk supports both. Sliding sync is faster but homeserver
support varies. **Decision**: let matrix-rust-sdk's `SlidingSyncBuilder`
default behavior pick. If the homeserver doesn't support it, the SDK
falls back to v2 sync automatically. No code in uplink decides.

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
  expires (Microsoft holds them ~7 days minimum, often much longer), the
  recovery path is "full re-sync". I assume our code can detect this
  (HTTP 410 Gone) and re-initialize without losing state. Worth verifying.

### 15.8 Storage capacity planning

Single user, btrfs, 50 GB target horizon. SQLite WAL on btrfs is fine
*provided* we set `nodatacow` on the DB directory (mandatory; CoW + WAL
fragmentation is pathological). `mail/` and `calendar/` should remain CoW
so reflink copies on snapshot don't double storage. Document this in
README install steps.

### 15.9 Irreversible external actions

**Decision**: sending is an outbox op (§6.7): logged like everything else,
undoable until performed, refused afterwards. Matrix redaction as an
"undo" after sending is best-effort at most (recipients may already have
the plaintext); if wanted, it would be an explicit action of its own.

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
| `frontend-lint`        | `pnpm lint` + `tsc --noEmit`                                  |
| `integration-gmail`    | NixOS VM test: uplink service + Gmail emulator                |
| `integration-m365`     | NixOS VM test: uplink service + Graph fake                    |
| `integration-matrix`   | NixOS VM test: uplink service + a real homeserver             |
| `e2e` *(future)*       | Reserved name; not wired in MVP                               |

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
  (RFC 8252 §7.3), so `uplink oauth init` can listen on an ephemeral port.
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
- **M365 (Graph):** emulate's Microsoft emulator covers OAuth only, no
  mail. Look for prior art at M5 start; otherwise a small Graph fake of
  the endpoints uplink uses (delta, `$value`, sendMail, PATCH/move).
- **Matrix:** Conduit (single-binary Rust homeserver, ships in nixpkgs).
  The test provisions it, registers two users, and gives uplink one.
  Synapse is the heavier fallback.

**Frontend testing — no Playwright in MVP**

- All frontend tests are vitest + jsdom against `@testing-library/preact`.
- The backend can be booted in-process from a vitest setup file so WS
  protocol tests run against a real backend talking to test fixtures.
- Every interactive element gets a stable `data-testid` attribute. Tests
  assert against testid + rendered text, not against component structure
  or class names — this lets agentic refactors restructure components
  without breaking tests.
- **Hook for later:** `checks.<system>.e2e` is reserved in the flake. The
  test runner is structured so that adding a Playwright leg later is
  additive — same backend fixtures, same data setup — not a refactor.
  Playwright in Nix requires pinning the browser binaries via
  `playwright-driver.browsers` and setting `PLAYWRIGHT_BROWSERS_PATH` +
  `PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1`; not done in MVP.

**Why no full browser E2E in MVP**

Browser-level rendering correctness is checked by hand during development.
This trades coverage for harness simplicity; acceptable for a single-user
tool. The risk is UI regressions from agentic refactors — mitigated by
stable testids and component-level integration tests covering rendered
output, but not eliminated. Revisit if regressions become a recurring
problem.

### 15.11 M365 push mechanism is a moving target

MVP M365 push is polling the Graph delta link every 30 s per folder
(§8.3). This is "good enough, not great" — real Outlook gets sub-second
push via MAPI HangingNotification. The eventual replacement for our
polling is EWS Streaming Notifications, which:

- works today (Outlook desktop's token includes `EWS.AccessAsUser.All`),
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

### 16.2 Route taxonomy

Routes drive which content view is active. The nav tree is always
visible. Routes (handled by `preact-iso`):

| Route                                  | Content pane                           |
|----------------------------------------|----------------------------------------|
| `/`                                    | Home / dashboard                       |
| `/mail/:account/:label`                | Message list (of a label, by provider id, e.g. `INBOX`) |
| `/mail/:account/:label/:entry`         | Message view (thread view once threads exist) |
| `/search/:terms`                       | Search results                         |
| `/search/:terms/:entry`                | Message view, within the results       |
| `/mail/compose/:draft`                 | Compose (new or continued draft)       |
| `/matrix/:account/:room`               | Room timeline                          |
| `/matrix/:account/space/:space`        | Space overview                         |
| `/calendar/:account`                   | Calendar (week/month)                  |
| `/todos`                               | Todo list                              |
| `/log`                                 | Transaction log (Undo UI)              |

### 16.3 Nav tree as a WS query

The nav tree is a single subscription on the WS protocol, distinct from
content-pane subscriptions. Nothing folder-specific in the wire shape —
the tree is just one query result, the active content pane is another.
The server pushes `NavTreeDelta` events when accounts come online,
folders change, Matrix spaces are joined/left, etc. No specialness in
the protocol for the nav tree.

### 16.4 Keyboard shortcuts

Hardcoded Gmail-derived bindings for MVP. No user-configurable keymap.
Cross-platform Cmd-vs-Ctrl handling and preset support are deferred to
roadmap; the source of truth is `web/src/keymap.ts`. `tinykeys` is the
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
| `clap`                    | CLI (`serve`, `keygen`, `oauth init`)                      |
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
| `@tiptap/core` + Preact compat | WYSIWYG compose                                       |
| `dompurify`               | Client-side HTML sanitization                              |
| `rrule`                   | Recurring event expansion                                  |
| `idb`                     | IndexedDB write-through                                    |
| `vitest` + `@testing-library/preact` | Tests                                           |

---

## Appendix B — Code review checklist

For every PR:

- [ ] New mutating code paths go through the `tx_log` dispatcher.
- [ ] New ops have an inverse, an `apply` that is atomic, and a `describe`.
- [ ] Mass ops produce *one* tx_log entry, not N.
- [ ] No `.eml` or `.ics` file is mutated or deleted by the application.
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
