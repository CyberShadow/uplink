-- An account's calendar, read-only (SPEC §10): each event as Graph lists
-- it in a calendar view, a single event or one occurrence of a series.
CREATE TABLE calendar_event (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id   TEXT NOT NULL REFERENCES account(id),
  external_id  TEXT NOT NULL,           -- Graph's id of the instance
  series       TEXT,                    -- Graph's seriesMasterId
  exception    INTEGER NOT NULL,        -- an occurrence changed from its series
  subject      TEXT NOT NULL,
  location     TEXT NOT NULL,
  preview      TEXT NOT NULL,
  start_ts     INTEGER NOT NULL,        -- ms since the epoch
  end_ts       INTEGER NOT NULL,
  all_day      INTEGER NOT NULL,
  raw          TEXT NOT NULL,           -- Graph's JSON of the instance
  UNIQUE (account_id, external_id)
);
CREATE INDEX calendar_event_by_start ON calendar_event(start_ts);

-- Where an account's calendar sync is (its window and delta link), apart
-- from its mail's.
CREATE TABLE calendar_sync (
  account_id  TEXT PRIMARY KEY REFERENCES account(id),
  state       TEXT NOT NULL
);
