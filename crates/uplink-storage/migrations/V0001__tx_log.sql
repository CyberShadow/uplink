-- Transaction log (SPEC §4.3, §6). Each entry records a forward op and the
-- inverse op computed against pre-state, inside the transaction that
-- applied the op.
CREATE TABLE tx_log (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  ts             INTEGER NOT NULL,          -- ms since epoch
  actor          TEXT NOT NULL,             -- 'user'
  description    TEXT NOT NULL,             -- human string, e.g. 'archived 12 messages'
  op_type        TEXT NOT NULL,             -- forward op kind
  op_json        TEXT NOT NULL,             -- forward op payload
  inverse_type   TEXT NOT NULL,             -- inverse op kind
  inverse_json   TEXT NOT NULL,             -- inverse op payload
  undone_by      INTEGER REFERENCES tx_log(id), -- set once this entry is undone
  undoes         INTEGER REFERENCES tx_log(id)  -- set if this entry is an undo
);
CREATE INDEX tx_log_by_ts ON tx_log(ts DESC);
CREATE INDEX tx_log_open ON tx_log(undone_by) WHERE undone_by IS NULL;
