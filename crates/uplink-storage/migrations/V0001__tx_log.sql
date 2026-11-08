-- The log (SPEC §4.3, §6): every change to uplink's state, in commit order.
-- User ops carry the inverse op computed when they were applied;
-- observations of external state (account_id set) have none. The log is
-- also the event stream: clients resume from an entry id.
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
