-- Drafts and the mail outbox (SPEC §6.7, §7).

-- A message being written. Local to uplink; edited only through draft ops,
-- so its history is in tx_log.
CREATE TABLE draft (
  id           TEXT PRIMARY KEY,   -- chosen by the client that created it
  account_id   TEXT NOT NULL,      -- configured account it is sent from
  revision     INTEGER NOT NULL,   -- edits so far; an edit names the one
                                   -- it is based on (§7.2)
  to_addrs     TEXT NOT NULL,      -- address lists as typed
  cc_addrs     TEXT NOT NULL,
  bcc_addrs    TEXT NOT NULL,
  subject      TEXT NOT NULL,
  body_html    TEXT NOT NULL,
  in_reply_to  INTEGER REFERENCES message(id),
  updated_at   INTEGER NOT NULL    -- unix ms
);

-- Mail queued to be sent. The account's sync task sends it once
-- not_before has passed; until then, the send can be undone. Ids are
-- never reused, since log entries refer to items by id.
CREATE TABLE mail_outbox (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id    TEXT NOT NULL,
  draft         TEXT NOT NULL,     -- JSON: the draft as sent
  message_id    TEXT NOT NULL UNIQUE, -- generated when queued, no brackets
  delay_ms      INTEGER NOT NULL,  -- the send delay it was queued with
  not_before    INTEGER NOT NULL,  -- unix ms
  state         TEXT NOT NULL CHECK (state IN ('queued', 'sending', 'sent', 'failed')),
  error         TEXT,              -- why it failed
  message       INTEGER REFERENCES message(id), -- once sent: the local copy
  CHECK ((state = 'failed') = (error IS NOT NULL)),
  CHECK ((state = 'sent') = (message IS NOT NULL))
);
CREATE INDEX mail_outbox_due ON mail_outbox(account_id, not_before) WHERE state = 'queued';
