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
- `/home/vladimir/work/.cydo/tasks/22672/output.md` — stack research (Rust,
  M365 Outlook-impersonation, hybrid storage, real-time transport).
- `/home/vladimir/work/.cydo/tasks/22673/output.md` — Matrix UI research
  (Option 1: build on uplink's WS protocol, with DOMPurify + markdown-it +
  emoji-mart + linkifyjs + ~600 LoC of Matrix-aware code).

---

## Table of contents

1. [Overview & invariants](#1-overview--invariants)
2. [Repository layout](#2-repository-layout)
3. [Configuration](#3-configuration)
4. [Storage & schema](#4-storage--schema)
5. [Wire protocol (WebSocket)](#5-wire-protocol-websocket)
6. [Transaction log & undo](#6-transaction-log--undo)
7. [Real-time draft sync](#7-real-time-draft-sync)
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
                        │ IMAP+IDLE    Graph    matrix-     todo   │
                        │   API send   delta    rust-sdk    /draft │
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
   HTTP for the SPA, WebSocket for live data, plus async tasks for IMAP IDLE
   and the matrix-rust-sdk sync loop.
2. **Server is the trust boundary.** All credentials, all OAuth refresh
   tokens, all Matrix decryption keys, all message bodies live server-side in
   plaintext at rest (encryption of OAuth secrets is at-rest, with the key
   in a sibling file — see §13). Client receives already-decrypted, already-
   resolved, ready-to-render payloads.
3. **Every state-mutating operation is dispatched through the tx_log.**
   Single chokepoint. Every dispatch records an op + inverse before applying.
   Mass operations are a *single* tx_log entry with a batch inverse. Undo is
   itself an op (append-only log).
4. **Raw `.eml` files and raw `.ics` blobs are sacred.** Content-addressed by
   SHA-256(raw bytes), never modified, never deleted by the application, no
   attachment extraction. Indexes (search, threading, labels) live in SQLite
   and are *derived* and rebuildable from the on-disk blobs.
5. **Real-time uses one channel: a JSON-over-WebSocket protocol uplink
   defines.** No SSE, no long-poll, no second port. Heartbeat is app-level
   ping/pong at 25–30 s. Reconnection replays missed events from a server-
   side cursor.
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
│   ├── uplink-email/                # Gmail+IMAP, M365 Graph mail; send
│   ├── uplink-matrix/               # matrix-rust-sdk wrapper
│   ├── uplink-calendar/             # M365 Graph calendar (read-only)
│   ├── uplink-todos/                # TodoBackend trait + local impl
│   ├── uplink-server/               # axum, TLS, basic auth, routes
│   └── uplink-cli/                  # `uplink matrix pair`, `uplink migrate`
├── web/                             # Preact SPA (Vite)
│   ├── package.json
│   ├── vite.config.ts               # mirrors cydo's proxy + auth pattern
│   ├── src/
│   │   ├── main.tsx
│   │   ├── connection.ts            # WS manager + reconnect
│   │   ├── stores/                  # @preact/signals stores
│   │   ├── views/                   # Mail, Matrix, Calendar, TODO
│   │   ├── components/
│   │   └── generated/protocol.ts    # produced by `cargo build` (ts-rs)
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
  boundary, and runs `ts-rs` to emit `web/src/generated/protocol.ts`. Keeps
  TS-side imports clean; nothing else can accidentally export a wire type.
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
    # Required scopes: https://mail.google.com/, gmail.send,
    #                  gmail.compose, openid email
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
    # uses authorization-code + PKCE through a CLI-driven flow once.
    # The Outlook client_id is the documented escape hatch on locked-down
    # tenants. Scopes are bundled into Outlook's implicit consent.
    oauth:
      client_id: "d3590ed6-52b3-4102-aeff-aad2292ab01c"
      authority: "https://login.microsoftonline.com/common"
      scopes:
        - "https://graph.microsoft.com/Mail.ReadWrite"
        - "https://graph.microsoft.com/Mail.Send"
        - "https://graph.microsoft.com/Calendars.Read"
        - "offline_access"
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
- One write connection, N read connections via a `tokio-rusqlite` pool.
- All writes serialized through the op dispatcher; read connections run in
  the tokio blocking pool.

**Why rusqlite over sqlx**: no compile-time DB schema dependency (sqlx
verifies queries against a live DB at build time, which is brittle for an
embedded DB with migrations and FTS5). `rusqlite` has first-class FTS5
support and integrates cleanly with `tokio-rusqlite`'s spawn_blocking model.
For a single-writer embedded DB, sqlx's pool semantics buy nothing.

**Migrations**: `refinery` crate. Forward-only, embedded SQL files in
`crates/uplink-storage/migrations/V0001__init.sql`, etc. The CLI command
`uplink migrate` runs them; the server runs them automatically at startup
and refuses to start if the migration set has been edited (checksum
mismatch).

### 4.3 Schema (initial migration sketch)

```sql
-- ── accounts (server-side reflection of config) ─────────────────────
CREATE TABLE account (
  id            TEXT PRIMARY KEY,           -- from YAML
  kind          TEXT NOT NULL,              -- 'gmail' | 'm365' | 'matrix'
  address       TEXT NOT NULL,              -- email or mxid
  sync_state    TEXT NOT NULL DEFAULT '{}', -- per-kind JSON state blob
                                            -- (IMAP UIDVALIDITY+UIDNEXT,
                                            --  Graph deltaLink, ...)
  enabled       INTEGER NOT NULL DEFAULT 1,
  created_at    INTEGER NOT NULL
);

-- ── messages (mail) ─────────────────────────────────────────────────
CREATE TABLE message (
  id              TEXT PRIMARY KEY,         -- sha256(.eml bytes), hex
  account_id      TEXT NOT NULL REFERENCES account(id),
  message_id      TEXT,                     -- RFC 5322 Message-ID header
  thread_id       TEXT,                     -- gmail thrid / msft conversationId
                                            -- or computed from References
  subject         TEXT,
  from_addr       TEXT,                     -- canonicalized addr
  from_name       TEXT,
  to_addrs        TEXT,                     -- JSON array
  cc_addrs        TEXT,
  bcc_addrs       TEXT,
  date_sent       INTEGER,                  -- unix seconds (Date: header)
  date_received   INTEGER NOT NULL,         -- unix seconds (server-arrived)
  size_bytes      INTEGER NOT NULL,
  flags           INTEGER NOT NULL,         -- bitfield (seen, flagged, ...)
  raw_path        TEXT NOT NULL             -- relative to $DATA, e.g.
                                            -- mail/ab/abcd....eml
);
CREATE INDEX message_by_thread ON message(thread_id, date_received DESC);
CREATE INDEX message_by_account_date ON message(account_id, date_received DESC);

-- FTS5 over extracted plain text. Body extraction is via html2text +
-- mail-parser stripping signatures and quoted replies (lightly).
CREATE VIRTUAL TABLE message_fts USING fts5(
  subject, from_addr, from_name, to_addrs, body_text,
  content='message', content_rowid='rowid',
  tokenize='unicode61 remove_diacritics 2'
);
-- Triggers keep message_fts in sync with INSERT/DELETE on message;
-- UPDATE not used (messages are immutable; flags/labels live in separate tables).
CREATE TRIGGER message_ai AFTER INSERT ON message BEGIN
  INSERT INTO message_fts(rowid, subject, from_addr, from_name, to_addrs, body_text)
  VALUES (new.rowid,
          new.subject,
          new.from_addr,
          coalesce(new.from_name, ''),
          new.to_addrs,
          ''); -- body_text filled by a follow-up UPDATE after parse
END;
CREATE TRIGGER message_ad AFTER DELETE ON message BEGIN
  DELETE FROM message_fts WHERE rowid = old.rowid;
END;

-- ── labels (Gmail-style, also used for M365 categories/folders) ─────
CREATE TABLE label (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id   TEXT NOT NULL REFERENCES account(id),
  name         TEXT NOT NULL,                  -- e.g. 'INBOX', 'Important'
  external_id  TEXT,                           -- gmail labelId, M365 folderId
  color        TEXT,                           -- '#rrggbb' or null
  UNIQUE(account_id, external_id)
);
CREATE TABLE message_label (
  message_id   TEXT NOT NULL REFERENCES message(id),
  label_id     INTEGER NOT NULL REFERENCES label(id),
  PRIMARY KEY (message_id, label_id)
);
CREATE INDEX message_label_by_label ON message_label(label_id, message_id);

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
  version        INTEGER NOT NULL DEFAULT 0,
  content_hash   BLOB NOT NULL,             -- sha256(canonical_json(fields))
  updated_at     INTEGER NOT NULL
);
CREATE TABLE draft_op (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  draft_id       TEXT NOT NULL REFERENCES draft(id),
  version        INTEGER NOT NULL,          -- the post-op version
  op_json        TEXT NOT NULL,
  applied_at     INTEGER NOT NULL,
  client_id      TEXT NOT NULL              -- which tab originated
);
CREATE INDEX draft_op_by_draft ON draft_op(draft_id, version);

-- ── transaction log ─────────────────────────────────────────────────
CREATE TABLE tx_log (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  ts             INTEGER NOT NULL,          -- ms since epoch
  actor          TEXT NOT NULL,             -- 'user' | account_id (for system ops)
  description    TEXT NOT NULL,             -- human string, e.g. 'archived 12 messages'
  op_type        TEXT NOT NULL,             -- discriminator
  op_json        TEXT NOT NULL,             -- forward op payload
  inverse_json   TEXT NOT NULL,             -- inverse op payload (recorded BEFORE apply)
  undone_by      INTEGER REFERENCES tx_log(id), -- if non-null, this op was undone
                                                -- by tx_log entry undone_by.
  undoes         INTEGER REFERENCES tx_log(id)  -- if non-null, this op is an undo
                                                -- of tx_log entry undoes.
);
CREATE INDEX tx_log_by_ts ON tx_log(ts DESC);
CREATE INDEX tx_log_open ON tx_log(undone_by) WHERE undone_by IS NULL;
```

### 4.4 Schema design notes

- **`message.id = sha256(.eml bytes)`.** Same bytes → same id, so re-fetching
  a message is idempotent. The same physical message replicated across IMAP
  folders shows up once in storage and is linked from multiple `message_label`
  rows.
- **No threads table.** `thread_id` is denormalized into `message`. Threading
  is whatever the provider tells us (Gmail `threadId`, M365 `conversationId`).
  We don't synthesize threads when the provider doesn't give them — MVP
  scope.
- **Flags are a bitfield.** Seen/Flagged/Answered/Draft fit comfortably.
  Provider-specific flags (gmail `\Important`, M365 `isRead`) project to
  these.
- **Soft delete is `message_label` row removal, not `message` deletion.** The
  raw `.eml` stays on disk forever (invariant 4). Hard delete is deferred —
  see ROADMAP.
- **`tx_log` carries forward AND inverse.** Computed inside the same SQLite
  transaction as the apply. The inverse is computed against pre-state, so
  it's correct *for the world that existed when the op was issued* — undo
  always replays that snapshot, not the current state.
- **`undone_by` / `undoes`** form pairs: when entry A is undone, a new entry
  B is appended with `B.undoes = A.id`, and a separate update sets
  `A.undone_by = B.id`. Both updates happen inside one transaction. Redo
  of an undo is a normal new op with `undoes = B.id`.

### 4.5 Body-text extraction for FTS

The IMAP/Graph fetch path writes the `.eml`, then enqueues a parse job
that:

1. Parses MIME with `mail-parser` (stalwartlabs).
2. Selects the `text/plain` part if present, otherwise the first
   `text/html` part run through `html2text` (the Rust crate, deterministic,
   no JS).
3. Updates `message_fts` with the extracted body text via a follow-up
   `UPDATE message_fts SET body_text = ?`.

Body extraction is deferred (not blocking the INSERT) so the inbox UI can
show new messages instantly while indexing catches up.

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
// crates/uplink-protocol/src/ws.rs

#[derive(Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum WsClientMsg {
    Ping { seq: u64 },
    /// First message after connection — tells server where we last were.
    Hello { cursor: Option<u64> },
    /// Request/response: the matching response carries the same request_id.
    Request {
        request_id: u64,
        #[serde(flatten)]
        body: ClientRequest,
    },
    /// Fire-and-forget client-originated events (draft edits primarily).
    Event {
        #[serde(flatten)]
        body: ClientEvent,
    },
}

#[derive(Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum WsServerMsg {
    Pong { seq: u64 },
    /// Server's reply to a Hello. Carries the assigned cursor and any
    /// initial state the client needs.
    Welcome { cursor: u64, accounts: Vec<AccountSummary> },
    /// Response to a Request.
    Response {
        request_id: u64,
        #[serde(flatten)]
        body: Result<ServerResponse, ServerError>,
    },
    /// Server-pushed event. Each carries a monotonic cursor for replay.
    Event {
        cursor: u64,
        #[serde(flatten)]
        body: ServerEvent,
    },
}
```

`ClientRequest`, `ClientEvent`, `ServerEvent`, `ServerResponse` are
themselves tagged enums; per-domain variants live in domain submodules
(`protocol::mail::*`, `protocol::matrix::*`, …) and are re-exported.

### 5.3 Replay buffer

The realtime crate keeps a ring buffer (e.g. 4096 most recent events) keyed
by cursor. On `Hello { cursor: Some(c) }`, the server replays events with
cursor `>c`. If `c` is older than the ring buffer's tail, server responds
with `Welcome { cursor }` carrying a "you must resync" marker and the
client refetches via REST.

The cursor is a per-server-process monotonic u64 — *not* persisted across
restarts. After a server restart, all clients full-resync. This is fine for
single-user.

### 5.4 Event types (MVP)

`ServerEvent` variants by domain:

- **Mail**: `MessageAdded`, `MessageRemoved`, `MessageFlagsChanged`,
  `LabelsChanged`, `ThreadChanged`.
- **Matrix**: `RoomAdded`, `RoomRemoved`, `RoomNameChanged`,
  `RoomUnreadChanged`, `MatrixEventAdded` (carries decrypted plain text +
  raw JSON for unknown types), `RoomReadMarkerChanged`.
- **Calendar**: `CalendarEventAdded`, `CalendarEventRemoved`,
  `CalendarEventChanged`.
- **TODO**: `TodoAdded`, `TodoChanged`, `TodoRemoved`.
- **Draft**: `DraftChanged { id, version, fields_set, by_client_id }`.
- **TxLog**: `TxLogEntryAdded`, `TxLogEntryUndone`.
- **Config**: `ConfigChanged { section }`.

`ClientRequest` variants (MVP):

- `Send { draft_id }` — submit a draft for sending.
- `Search { query, scope }` — FTS5 cross-account search.
- `LoadMessage { id }` — fetch full body (text + sanitized HTML).
- `LoadRoomTimeline { room_id, before_event_id, limit }`.
- `Undo { tx_log_id }`.
- `OpenDraft { id }` / `CreateDraft { account_id, ... }`.

`ClientEvent` variants (MVP):

- `DraftEdit { id, base_hash, op }` — see §7.

---

## 6. Transaction log & undo

### 6.1 The dispatcher

A single function with this shape:

```rust
pub trait Op: Sized {
    /// JSON discriminator key. e.g. "mail.archive" / "matrix.send_message".
    const KIND: &'static str;

    /// Apply self atomically. Compute the inverse from pre-state. Return
    /// (inverse, broadcast_events). On error, the transaction rolls back.
    fn apply(
        self,
        tx: &mut rusqlite::Transaction,
    ) -> Result<(InverseOp, Vec<ServerEvent>)>;

    /// Human description for the tx_log UI.
    fn describe(&self) -> String;
}

pub fn dispatch<O: Op + Serialize>(
    ctx: &Ctx,
    op: O,
) -> Result<TxLogId> {
    let db = ctx.db.write().lock();
    let mut tx = db.transaction()?;

    let op_json = serde_json::to_string(&op)?;
    let description = op.describe();

    let (inverse, events) = op.apply(&mut tx)?;
    let inverse_json = serde_json::to_string(&inverse.body)?;

    let id = tx.query_row(
        "INSERT INTO tx_log (ts, actor, description, op_type, op_json, inverse_json)
         VALUES (?, ?, ?, ?, ?, ?)
         RETURNING id",
        params![now_ms(), "user", description, O::KIND, op_json, inverse_json],
        |r| r.get::<_, i64>(0),
    )?;
    tx.commit()?;

    // After commit, broadcast events and the new tx_log entry.
    for ev in events { ctx.hub.broadcast(ev); }
    ctx.hub.broadcast(ServerEvent::TxLogEntryAdded { id, .. });

    Ok(TxLogId(id))
}
```

Key properties:

- **Op application and tx_log INSERT are in the same SQLite transaction.**
  If the op fails, no log entry; if the log INSERT fails, the op rolls back.
  Atomic.
- **Broadcast happens after commit.** Subscribers see only committed state.
- **The inverse is computed inside `apply`** against pre-state, before
  pre-state is mutated. This means undo replays the inverse against
  *current* state — but the inverse was computed knowing what to put back.
  For mass operations this is one inverse describing all the per-row changes.
- **Op author writes the inverse.** No automatic introspection. Op-specific
  knowledge of which fields changed is encoded in `apply`.

### 6.2 InverseOp

```rust
pub struct InverseOp {
    pub kind: String,        // "mail.archive.inverse" etc.
    pub body: InverseBody,   // tagged enum, serializable
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InverseBody {
    MailFlagsRestore { entries: Vec<(MessageId, Flags, Vec<LabelId>)> },
    MatrixSendRedact { event_id: String, room_id: String },
    TodoRecreate { todo: TodoRow },
    TodoSetDone { id: i64, done_at: Option<i64> },
    DraftSetFields { id: DraftId, version: u64, fields: DraftFields },
    // ...
}
```

Notably, **the inverse of "send a Matrix message" is "redact the message"**
(not "delete it locally"). Inverses must be operations that themselves make
sense in the external world. This is exactly the property that makes undo
useful: it has the same blast radius as the original op.

### 6.3 Undo

The `Undo { tx_log_id }` request:

1. Look up tx_log entry by id.
2. If `entry.undone_by IS NOT NULL`, error: already undone.
3. Construct an `Op` from `entry.inverse_json` (a `KIND` lookup over a
   registry).
4. Dispatch it. Inside its `apply`, it sets `tx_log[entry_id].undone_by =
   new_id` *and* the new entry has `undoes = entry_id`.

Redo of an undo is just "undo the undo entry". This means the user can
toggle back and forth.

### 6.4 Mass operations

A mass operation (e.g. "archive 50 selected messages") is a *single* Op,
not 50 ops. Its `apply` mutates 50 rows, computes a single inverse listing
all 50 pre-states, and produces one tx_log entry. The UI shows it as
"Archived 50 messages" with one Undo button.

Without this property, the undo log would be flooded with per-row noise
and undo would have to replay 50 entries to roll back one user gesture.

### 6.5 Which operations go through the dispatcher

**Everything user-initiated that mutates persistent state.** Examples:

- Marking message read/unread, archiving, labeling, deleting.
- Sending a Matrix message.
- Creating, editing, completing a TODO.
- Flagging a message for follow-up (creates a linked TODO).

**Not** through the dispatcher:

- Background sync writes (new mail arriving, Matrix events streaming in,
  calendar delta). These are *observations* of the external world; they
  aren't user actions and can't be undone in any meaningful sense. They go
  into the broadcast hub directly and write to storage via a dedicated
  ingest path that does *not* touch `tx_log`.
- Read-only queries (search, load message body, load room timeline).
- Draft edits (different sync mechanism — see §7).

### 6.6 Why drafts aren't tx_log ops

Drafts get edited dozens of times per minute. Putting each keystroke in
the tx_log would (a) bloat it past usefulness and (b) make undo confusing
("undo my last keystroke"). Drafts have their own CAS edit protocol with
their own op log (`draft_op` table) used for replay across tabs.

---

## 7. Real-time draft sync

### 7.1 What we actually need

Single user, two tabs (or laptop+phone, eventually). Tab A is typing in the
compose window, tab B has the same draft open. We want:

- Tab B sees tab A's edits within ~500 ms.
- If both edit at once, neither's edit is silently dropped.
- If the network drops, tab A's keystrokes survive locally and flush on
  reconnect.

We do not need: collaborative editing primitives (no two-user concurrent
typing), operational transforms, CRDTs, presence (no second user).

### 7.2 Server-arbitrated CAS

Each draft has `version: u64` and `content_hash: [u8; 32]` (sha256 of the
canonical-JSON of all draft fields, via `serde_jcs` for stable ordering).

Client sends:

```jsonc
{
  "type": "event",
  "body": {
    "type": "draft_edit",
    "id": "01H...",
    "base_hash": "abcd...32-byte-hex",
    "op": {
      "type": "field_set",
      "field": "subject",
      "value": "Re: hello"
    },
    "client_id": "tab-uuid-1"
  }
}
```

Server logic:

1. Load `draft` row, compute current `content_hash`.
2. If `current_hash == base_hash`: apply op, recompute hash and version+1,
   write `draft_op` row, broadcast `DraftChanged { ... }` to all subscribers
   (including the originating tab — which uses `client_id` to recognize its
   own echo).
3. If `current_hash != base_hash`: send `Response { error: stale_base }` to
   the originating tab. Tab refreshes draft state and retries.

### 7.3 DraftOp variants

```rust
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DraftOp {
    /// Set a top-level field. Used for subject, to/cc/bcc, in_reply_to.
    FieldSet { field: DraftField, value: serde_json::Value },
    /// Insert text into body_text at offset. Used for compose typing.
    Insert { field: BodyField, offset: u32, text: String },
    /// Delete a range from body_text. Used for compose deletes.
    Delete { field: BodyField, offset: u32, length: u32 },
    /// Atomic replace (used by WYSIWYG body_html — TipTap outputs HTML
    /// in one shot; we don't try to diff it).
    Replace { field: BodyField, value: String },
}
```

Insert/Delete on `body_text` lets short edits avoid sending the whole body.
For `body_html` we use `Replace` because WYSIWYG output isn't safe to
incremental-diff.

### 7.4 Coalescing

Client batches keystrokes for ~150 ms before sending a single `Insert` /
`Delete`. The intra-tab UX still feels instant (local state updates on
keypress, network echo confirms it).

### 7.5 IndexedDB write-through

Every keystroke writes to IndexedDB *before* attempting the WS send. On
reconnect, the client's queue is replayed. On client startup, IndexedDB is
inspected: if there's a draft state newer than the server's last-known
version for that draft, the client diffs locally and sends the missing ops.

This handles the "network drops for 5 minutes mid-paragraph" case without
data loss.

### 7.6 Why hashing not version numbers

If we keyed CAS on `(draft_id, base_version)`, two tabs could each send a
patch off the same base_version, the server would apply one, and the second
would see `current_version > base_version` and reject. Fine. But `base_hash`
is more robust against the edge case where the server has applied an op
that *happened to produce the same fields* — e.g. tab A inserts "x" and
deletes "x" in two quick gestures; tab B's edit racing in between should
still be rejected only if content actually differs. Hashes are content-
identity, versions are commit-identity. Content-identity is the right
primitive.

### 7.7 Bandwidth cost

`serde_jcs` runs over the whole draft on every edit. For a 50 KB body
that's ~50 KB hashed per keystroke (or per coalesced batch). Sha256 of
50 KB is microseconds. Negligible.

---

## 8. Email pipeline (Gmail + M365)

### 8.1 Gmail — receive

- **Transport**: IMAP+XOAUTH2 against `imap.gmail.com:993` (TLS).
  Library: `async-imap`. SASL XOAUTH2 mechanism with an OAuth access token
  refreshed from `oauth2` crate.
- **Idle**: subscribe to INBOX (and all labels we want streaming for) using
  `IDLE`. Reconnect on `BYE` / network drop with exponential backoff. Run
  one IDLE per Gmail label we follow (INBOX + a configured allowlist;
  default just INBOX).
- **Initial sync**: on first launch, `UID FETCH 1:* (UID FLAGS INTERNALDATE
  RFC822.HEADER X-GM-LABELS X-GM-MSGID X-GM-THRID)` to walk the entire
  mailbox metadata-first; then fetch bodies of the most-recent N messages
  fully and lazy-load the rest.
- **Incremental**: on each IDLE event, fetch the new UIDs. Persist
  `UIDVALIDITY` and last seen `UIDNEXT` in `account.sync_state`.
- **Threading**: use Gmail's `X-GM-THRID` directly.
- **Labels**: use Gmail's `X-GM-LABELS`. Each label becomes a `label` row;
  membership goes in `message_label`.

### 8.2 Gmail — send

- **Transport**: Gmail API `users.messages.send`. Build RFC 5322 message
  in-memory, base64url-encode it, POST.
- **Why API not SMTP**: avoids "Sent" folder synchronization mismatch
  (Gmail SMTP doesn't append to Sent; the API does).
- **Library**: hand-rolled HTTPS+JSON via `reqwest`. No SDK; Gmail send is
  ~30 lines of code.
- **OAuth scope required**: `https://www.googleapis.com/auth/gmail.send`.

### 8.3 M365 — receive

- **Transport**: Microsoft Graph `GET /me/mailFolders/inbox/messages/delta`
  on first call, then `@odata.deltaLink` for subsequent calls. Library:
  `reqwest` + `oauth2`. No Graph SDK (none of the Rust ones are first-
  party and Graph is JSON+OAuth, easy to call directly).
- **Polling**: every 60 s (no IDLE on Graph). Could be reduced if we set
  up subscriptions (push), but subscriptions require an HTTPS endpoint
  reachable from Microsoft. Deferred.
- **Threading**: use `conversationId` from each message.
- **Folders**: Graph exposes mailFolders; each folder becomes a `label` row.
  Categories (`categories: ["red", "blue"]`) become additional labels
  prefixed with `cat:`.
- **OAuth**: Outlook desktop client_id impersonation
  (`d3590ed6-52b3-4102-aeff-aad2292ab01c`). PKCE auth-code flow. See §13.

### 8.4 M365 — send

- **Transport**: `POST /me/sendMail` with JSON message body. Graph builds
  the RFC 5322 server-side.
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

After successful send:

1. Persist the assembled RFC 5322 bytes to `mail/<sha256>/<sha256>.eml`.
2. Insert `message` row with the assigned account's "Sent" label.
3. Broadcast `MessageAdded`.

This is the only path that writes to the mail store from compose. The
provider's "Sent" folder will *also* show it (Gmail API auto-appends to
Sent; Graph likewise). Next sync pass we'll observe it again — the
sha256 content-addressing means it dedups against the local copy
automatically (same bytes → same `message.id`).

### 8.7 Body extraction & FTS

After every mail INSERT, enqueue an extract-body task (tokio task). It:

1. Reads the `.eml` from disk.
2. Parses with `mail-parser`.
3. Selects best `text/plain` or runs html2text over the HTML part.
4. Truncates to 64 KB.
5. `UPDATE message_fts SET body_text = ? WHERE rowid = ?`.

If body extraction fails (rare; mail-parser is permissive), the message is
still in the FTS index by subject/from/to but body_text is empty. Log and
move on.

### 8.8 Error semantics

- **OAuth refresh fails** (revoked token, password changed): set
  `account.enabled = 0`, broadcast `AccountListChanged` with an error flag,
  surface in UI as "Reauthenticate <account>".
- **IMAP disconnects**: reconnect with backoff (1s, 2s, 4s, 8s, 16s, 30s,
  60s cap). Log warning at 5 m of continuous failure.
- **Graph 429**: respect `Retry-After`. Backoff in the same units.

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

Send is a tx_log op (`MatrixSendMessage`). Its inverse is
`MatrixRedactEvent { event_id, room_id }`. On dispatch:

1. Insert a placeholder `matrix_event` row marked `pending = 1` (an extra
   column not shown above; flag for "local echo").
2. Broadcast `MatrixEventAdded` with the local echo.
3. After server ACK, UPDATE the row replacing the temp id with the canonical
   `event_id`, clear `pending`, broadcast `MatrixEventUpdated`.
4. If send fails, mark the row `failed = 1`, surface in UI.

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

Mirroring the cydo pattern (proxy + auth forwarding):

```ts
// web/vite.config.ts
import { defineConfig } from 'vite'
import preact from '@preact/preset-vite'

export default defineConfig({
  plugins: [preact()],
  server: {
    port: 5173,
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:8080', // uplink dev server (HTTP, no TLS)
        changeOrigin: true,
        // Forward the same basic-auth header in dev so authenticated routes
        // work without the user re-entering credentials.
      },
      '/ws': {
        target: 'ws://127.0.0.1:8080',
        ws: true,
        changeOrigin: true,
      },
    },
  },
  build: {
    outDir: '../crates/uplink-server/static',
    emptyOutDir: true,
  },
})
```

In production, the Rust binary serves `static/` and the SPA's WS connects
to the same origin.

### 12.3 Connection manager

`web/src/connection.ts` — single class wrapping the WebSocket with:

- Exponential reconnect (250 ms, 500, 1 s, 2 s, 4 s, 8 s, capped at 30 s).
- `visibilitychange` listener that triggers immediate heartbeat on
  visible-after-hidden.
- Outbound message queue, drained on connect.
- Cursor tracking: every received `Event { cursor }` updates a local
  cursor; on reconnect, `Hello { cursor }` is sent.
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

- `j`/`k` next/prev message.
- `gi`/`gs`/`gd`/`gt`/`ga` jump to Inbox / Starred / Drafts / Sent / All.
- `c` compose, `r` reply, `a` reply-all, `f` forward.
- `e` archive, `#` delete, `!` mark as spam, `s` star.
- `/` focus search.
- `?` show shortcut overlay.
- `u` back to list.

The notable choices we are NOT picking:

- **No `z` for undo.** Undo lives in the transaction log view and as an
  optional toast immediately after each mutating op. `z` overloads
  unhelpfully when search/compose has focus, and the tx_log is the
  better surface for the multi-step undo we actually support.

### 12.6 Bundle budget

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
  YAML. See §15.2.
- One-time setup: `uplink oauth init --account=alice-gmail`. The CLI
  prints an authorization URL with PKCE, the user opens it in a browser,
  Google issues an authorization code, the user pastes the code back into
  the CLI, the CLI exchanges code for refresh+access token, seals the
  refresh token (see §13.4), writes it to
  `accounts/alice-gmail.token`.
- Runtime: server reads sealed token at startup, unseals with `master_key`,
  uses `oauth2` crate to refresh on each access-token expiry.

### 13.3 OAuth flow (M365 with Outlook impersonation)

- Same shape as Gmail. `client_id =
  d3590ed6-52b3-4102-aeff-aad2292ab01c`, no `client_secret`.
- Authority `https://login.microsoftonline.com/common`.
- Redirect URI `http://localhost:<random-port>/oauth-callback`. The CLI
  opens an ephemeral local HTTP server to catch the redirect, then shuts
  it down. Avoids the user having to paste a code.
- Scopes per §3.2.
- Refresh tokens rotate; we always write the latest sealed token back.

### 13.4 Sealing OAuth secrets at rest

- `master.key` is a 32-byte file generated once by `uplink keygen` and
  stored at `master_key_file` (default `/etc/uplink/master.key`, mode 0400
  owned by the uplink user).
- Token files contain `chacha20poly1305(master_key, nonce, refresh_token)
  || nonce`. Library: `chacha20poly1305` crate (RustCrypto).
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

- IMAP+XOAUTH2 connect + IDLE.
- Initial UID walk + metadata insert.
- Body fetch + .eml persist + FTS body extract.
- Frontend mail-list view with selection + keyboard nav.

**Done when:** a freshly-paired Gmail account streams new messages into
the UI within ~5 s of arrival on IMAP; FTS search returns hits across
subject/body/from; a power-loss simulation (kill + restart server) doesn't
lose any persisted messages.

### M5. M365 receive

- OAuth2 auth-code+PKCE with Outlook-client impersonation.
- Graph delta sync for mail.
- Same .eml persist + FTS pipeline.

**Done when:** an M365 account, paired via the CLI, streams its messages
into the unified inbox alongside Gmail; the UI cleanly distinguishes
which account each message is from; the delta-link survives restart.

### M6. Send + Compose + local Drafts

- TipTap compose UI.
- Local-only drafts with CAS protocol.
- Gmail API send.
- Graph sendMail.
- Tx_log op for send (inverse: nothing in MVP — once it's out, it's out).

**Done when:** composing a reply, switching tabs mid-edit, and having the
draft survive a network drop all work; clicking send actually transmits;
the sent message shows up in the local mail store within one provider-
sync interval *and* dedups by sha256 against the local copy on round-
trip.

### M7. Matrix

- Account pair CLI subcommand (Secret Storage import).
- matrix-rust-sdk sync loop.
- Plain-text receive into `matrix_event`.
- Plain-text send via dispatch op (inverse: redact).
- Room list + timeline UI.
- Unknown event types rendered as raw JSON.

**Done when:** sending a `m.text` to a room from uplink appears in
another Matrix client; sending one from the other client appears in
uplink; undoing the send produces a visible redaction event; an unknown
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
- Gmail (M4) precedes M365 (M5) because IMAP+IDLE is more straightforward
  than the OAuth-impersonation dance, so we hit fewer category errors at
  the start.
- Matrix (M7) precedes Calendar (M8) because Matrix exercises the
  irreversible-op codepath (redact-as-inverse) which is the more complex
  half of the tx_log story; getting that right early prevents revising
  the Op trait late.
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

Gmail API + IMAP+XOAUTH2 require a Google Cloud project with OAuth client
credentials registered in Google Cloud Console. There is no
fully-decentralised path — Google rejects plaintext IMAP passwords and
OAuth requires a registered client. Two patterns are viable:

1. **BYO (MVP).** The user creates a one-off GCP project (~10 minutes in
   the Cloud Console), marks it as a Desktop/installed app, and pastes
   `client_id` + `client_secret` into uplink's YAML. Each install has its
   own OAuth project; the upstream uplink project has zero ongoing
   relationship with Google. This is what we ship in MVP.
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
to a tenant-registered Entra app or to IMAP+XOAUTH2 with minimal
disruption. Do not couple anything but the OAuth crate to the client_id.

### 15.4 Refresh-token rotation under Outlook impersonation

Microsoft sometimes rotates refresh tokens on every refresh; sometimes
not. The `oauth2` crate handles either, but our token-file persistence
must atomically replace the token on every refresh. **Decision**: write
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

### 15.9 Inverse semantics for irreversible external actions

Sending a Matrix message is *de facto* irreversible (redact is best-
effort; recipients may have already cached the plaintext). For Gmail/M365
send, the op has no inverse — once it's out, it's out. The tx_log entry
records the send (preserving the audit trail) and the Undo button in the
log UI is rendered **disabled** (not clickable; not a clickable no-op)
with a tooltip "cannot un-send mail". The alternative — don't log
irreversible ops at all — loses the audit trail; we prefer the trail plus
a disabled Undo control.

### 15.10 Test infrastructure

CI runs exactly `nix flake check`. No separate workflow YAML to drift; the
flake is the source of truth. Mirrors the pattern in `../cydo`.

**Flake checks (each runs independently and parallel-friendly):**

| Check                  | What it runs                                                  |
|------------------------|---------------------------------------------------------------|
| `backend-test`         | `cargo test --workspace`                                      |
| `backend-lint`         | `cargo clippy --workspace -- -D warnings` + `cargo fmt --check` |
| `frontend-test`        | `pnpm test` (vitest + jsdom + `@testing-library/preact`)      |
| `frontend-lint`        | `pnpm lint` + `tsc --noEmit`                                  |
| `integration-mail`     | Backend talks to an embedded IMAP/SMTP server fixture         |
| `integration-matrix`   | Backend talks to a minimal homeserver fixture                 |
| `integration-graph`    | Backend talks to `wiremock-rs` recording of Graph             |
| `e2e` *(future)*       | Reserved name; not wired in MVP                               |

**Unit / property tests**

- Rust integration tests use a temporary `$DATA` directory + a fresh
  SQLite DB per test. Fixture helper provisions migrations, master key,
  and a stub `Config`.
- `rstest` for parameterized fixtures.
- Op-trait tests assert apply → inverse → apply round-trips for every
  Op variant.

**Integration fixtures (in-process, no Docker)**

- **IMAP+SMTP:** GreenMail (well-packaged in nixpkgs as a Java jar)
  launched as a flake-built test fixture; backend connects to
  `localhost:<random_port>`. Stalwart-mail is the Rust alternative and
  becomes preferable if we hit GreenMail compatibility limits — pick
  during the M1 spike.
- **Matrix:** Conduit (single-binary Rust homeserver, ships in nixpkgs).
  A test fixture provisions the homeserver, registers two users, and
  exposes their access tokens to tests. Synapse is the heavier fallback.
- **Graph / Gmail API:** `wiremock-rs` (preferred over `mockito` because
  it handles streaming/chunked responses, which Graph uses). Worth a 1-h
  spike at M4 start to confirm coverage of multi-call expectations.

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
| `/mail/:account/:folder`               | Thread list                            |
| `/mail/:account/:folder/:thread`       | Thread view                            |
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
| `tokio-rusqlite`          | Async wrapper                                              |
| `refinery`                | Migrations                                                 |
| `serde`/`serde_json`      | Serde                                                      |
| `serde_jcs`               | RFC 8785 canonical JSON for draft hashing                  |
| `ts-rs`                   | Rust → TypeScript type codegen                             |
| `mail-parser`             | RFC 5322 + MIME parsing                                    |
| `mail-builder`            | Outbound MIME assembly                                     |
| `html2text`               | HTML→plaintext for FTS body extraction                     |
| `ammonia`                 | Server-side HTML sanitizer                                 |
| `async-imap`              | IMAP+XOAUTH2 with IDLE                                     |
| `reqwest` + `oauth2`      | HTTPS + OAuth                                              |
| `matrix-sdk`              | Matrix client with E2EE                                    |
| `notify`                  | inotify config reload                                      |
| `chacha20poly1305`        | OAuth token sealing                                        |
| `bcrypt`                  | Basic-auth password verification                           |
| `subtle`                  | Constant-time comparison                                   |
| `ulid`                    | ULID for draft ids                                         |
| `tracing`+`tracing-subscriber` | Structured logging                                    |
| `anyhow`/`thiserror`      | Errors                                                     |
| `wiremock`                | Test HTTP fakes                                            |
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
