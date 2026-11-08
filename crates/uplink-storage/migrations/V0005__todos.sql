-- To-dos (SPEC §11): local to uplink, changed only through todo ops, so
-- their history is in tx_log. Ids are never reused, since log entries
-- refer to them.
CREATE TABLE todo (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  title       TEXT NOT NULL,
  notes       TEXT NOT NULL,
  due         TEXT,                  -- a date, YYYY-MM-DD
  done_at     INTEGER,               -- unix ms; NULL while open
  -- The message it was flagged from (§11.2): the message is flagged while
  -- the to-do is open.
  message     INTEGER REFERENCES message(id),
  created_at  INTEGER NOT NULL,      -- unix ms
  updated_at  INTEGER NOT NULL
);
CREATE INDEX todo_by_message ON todo(message) WHERE message IS NOT NULL;
