-- Events the user creates, changes and deletes (SPEC §10.4), as desired
-- state (§6.6): `desired` is what the user asked for that the provider
-- does not have yet (JSON: an event to create, fields changed, or its
-- deletion), made by the op `desired_by`, or refused by the provider
-- (`desired_refused`, why), which waits for a decision. An event's
-- columns are as shown, the change over what the provider has (`raw`).
--
-- A series' master, which calendar views do not list, is kept apart
-- (`calendar_series`): its JSON, fetched as its occurrences are seen,
-- says how it recurs; a change to the series is desired of it.
--
-- An event or a series created by uplink has no `external_id` until the
-- provider has it; its `transaction_id`, Graph's, finds it there should
-- the outcome of creating it be unknown.

CREATE TABLE calendar_series (
  id               INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id       TEXT NOT NULL REFERENCES account(id),
  external_id      TEXT,              -- Graph's id of the master
  transaction_id   TEXT,
  raw              TEXT,              -- Graph's JSON of the master, once fetched
  desired          TEXT CHECK (desired IS NULL OR json_valid(desired)),
  desired_by       INTEGER REFERENCES tx_log(id) DEFERRABLE INITIALLY DEFERRED,
  desired_refused  TEXT,
  CHECK ((desired IS NULL) = (desired_by IS NULL)),
  CHECK (desired_refused IS NULL OR desired IS NOT NULL),
  CHECK (external_id IS NOT NULL OR (transaction_id IS NOT NULL AND desired IS NOT NULL)),
  CHECK (raw IS NULL OR external_id IS NOT NULL),
  UNIQUE (account_id, external_id),
  UNIQUE (account_id, transaction_id)
);

INSERT INTO calendar_series (account_id, external_id)
SELECT DISTINCT account_id, series FROM calendar_event WHERE series IS NOT NULL;

CREATE TABLE calendar_event_new (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id        TEXT NOT NULL REFERENCES account(id),
  external_id       TEXT,             -- Graph's id of the instance
  transaction_id    TEXT,
  series_id         INTEGER REFERENCES calendar_series(id),
  exception         INTEGER NOT NULL, -- an occurrence changed from its series
  subject           TEXT NOT NULL,
  location          TEXT NOT NULL,
  preview           TEXT NOT NULL,
  start_ts          INTEGER NOT NULL, -- ms since the epoch
  end_ts            INTEGER NOT NULL,
  all_day           INTEGER NOT NULL,
  cancelled         INTEGER NOT NULL,
  show_as           TEXT NOT NULL,
  response          TEXT NOT NULL,
  join_url          TEXT,
  reminder          INTEGER,
  modified_ts       INTEGER NOT NULL, -- when it last changed (V0051)
  raw               TEXT,             -- Graph's JSON of the instance
  desired_response  TEXT,
  response_by       INTEGER REFERENCES tx_log(id) DEFERRABLE INITIALLY DEFERRED
    CHECK ((desired_response IS NULL) = (response_by IS NULL)),
  response_refused  TEXT CHECK (response_refused IS NULL OR desired_response IS NOT NULL),
  desired           TEXT CHECK (desired IS NULL OR json_valid(desired)),
  desired_by        INTEGER REFERENCES tx_log(id) DEFERRABLE INITIALLY DEFERRED,
  desired_refused   TEXT,
  CHECK ((desired IS NULL) = (desired_by IS NULL)),
  CHECK (desired_refused IS NULL OR desired IS NOT NULL),
  -- At the provider, or being created: of itself, or as its series is.
  CHECK ((external_id IS NULL) = (raw IS NULL)),
  CHECK (external_id IS NOT NULL OR (transaction_id IS NOT NULL AND desired IS NOT NULL)
         OR series_id IS NOT NULL),
  UNIQUE (account_id, external_id),
  UNIQUE (account_id, transaction_id)
);

INSERT INTO calendar_event_new
  (id, account_id, external_id, series_id, exception, subject, location, preview,
   start_ts, end_ts, all_day, cancelled, show_as, response, join_url, reminder, modified_ts,
   raw, desired_response, response_by, response_refused)
SELECT e.id, e.account_id, e.external_id, s.id, e.exception, e.subject, e.location, e.preview,
       e.start_ts, e.end_ts, e.all_day, e.cancelled, e.show_as, e.response, e.join_url,
       e.reminder, e.modified_ts, e.raw, e.desired_response, e.response_by, e.response_refused
FROM calendar_event e
LEFT JOIN calendar_series s ON s.account_id = e.account_id AND s.external_id = e.series;

-- Ids are never reused: log entries and marks of what was seen (V0042)
-- refer to them. Renaming the table renames its sequence.
DELETE FROM sqlite_sequence WHERE name = 'calendar_event_new';
INSERT INTO sqlite_sequence (name, seq)
SELECT 'calendar_event_new', seq FROM sqlite_sequence WHERE name = 'calendar_event';
DROP TABLE calendar_event;
ALTER TABLE calendar_event_new RENAME TO calendar_event;
CREATE INDEX calendar_event_by_start ON calendar_event(start_ts);
CREATE INDEX calendar_event_pending ON calendar_event(account_id) WHERE desired_response IS NOT NULL;
CREATE INDEX calendar_event_changing ON calendar_event(account_id) WHERE desired IS NOT NULL;
CREATE INDEX calendar_event_of_series ON calendar_event(series_id);
