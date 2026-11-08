-- Matrix (SPEC §4.3, §9): the rooms and events the account's sync task
-- has observed, and messages being sent. Ids are local and never reused,
-- since log entries refer to them.

CREATE TABLE matrix_room (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id   TEXT NOT NULL REFERENCES account(id),
  room_id      TEXT NOT NULL,        -- !opaque:server
  name         TEXT NOT NULL,        -- display name, as the SDK computes it
  encrypted    INTEGER NOT NULL,
  membership   TEXT NOT NULL CHECK (membership IN ('joined', 'invited', 'left')),
  -- The read marker: events up to and including this one are read.
  -- Desired (set by ops and by receipts from other devices), and the
  -- latest receipt sent or observed, which only moves forward (§9.5).
  read_up_to   INTEGER REFERENCES matrix_event(id),
  read_pushed  INTEGER REFERENCES matrix_event(id),
  UNIQUE (account_id, room_id)
);

-- Every timeline event, in the order sync delivered them. Events of
-- types uplink does not display are kept too, and shown as raw JSON.
CREATE TABLE matrix_event (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  room         INTEGER NOT NULL REFERENCES matrix_room(id),
  event_id     TEXT NOT NULL,        -- $opaque
  sender       TEXT NOT NULL,        -- mxid
  sender_name  TEXT,                 -- display name in the room, if set
  ts           INTEGER NOT NULL,     -- origin_server_ts, unix ms
  type         TEXT NOT NULL,        -- e.g. 'm.room.message'
  msgtype      TEXT,                 -- of m.room.message: 'm.text', …
  body         TEXT,                 -- of m.room.message: the plain text
  raw_json     TEXT NOT NULL,        -- the event, decrypted if it could be
  encryption   TEXT NOT NULL CHECK (encryption IN ('none', 'decrypted', 'undecryptable')),
  UNIQUE (room, event_id)
);
CREATE INDEX matrix_event_by_room ON matrix_event(room, id);

-- Messages being sent (§6.7, §9.3). Sent as transaction 'uplink-<id>',
-- so a retry cannot duplicate one. Deleted once the event is synced.
CREATE TABLE matrix_outbox (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  room         INTEGER NOT NULL REFERENCES matrix_room(id),
  body         TEXT NOT NULL,
  queued_at    INTEGER NOT NULL,     -- unix ms
  state        TEXT NOT NULL CHECK (state IN ('queued', 'sending', 'sent', 'failed')),
  error        TEXT,                 -- why the homeserver refused it
  event_id     TEXT,                 -- once sent
  CHECK ((state = 'failed') = (error IS NOT NULL)),
  CHECK ((state = 'sent') = (event_id IS NOT NULL))
);
