-- Mail (SPEC §4.3–4.4).

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
-- An account may hold several copies of one logical message. Entry and
-- label ids are never reused, since log entries refer to them.
CREATE TABLE mail_entry (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
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
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
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
