-- A Matrix send may change the room's members (SPEC §9.3), which the
-- homeserver does without saying the event it made: sent, such a send has
-- no event id, and is synced at once.

CREATE TABLE matrix_send_new (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  room        INTEGER NOT NULL REFERENCES matrix_room(id),
  content     TEXT NOT NULL CHECK (json_valid(content)),
  queued_at   INTEGER NOT NULL,  -- unix ms
  wanted      INTEGER NOT NULL,  -- desired: 0 once cancelled
  desired_by  INTEGER NOT NULL REFERENCES tx_log(id) DEFERRABLE INITIALLY DEFERRED,
  state       TEXT NOT NULL CHECK (state IN ('queued', 'sending', 'sent', 'refused')),
  error       TEXT,              -- why the homeserver refused it
  event_id    TEXT,              -- once sent, if the homeserver said
  synced      INTEGER NOT NULL,  -- its event is synced
  CHECK ((state = 'refused') = (error IS NOT NULL)),
  CHECK (event_id IS NULL OR state = 'sent'),
  CHECK (state <> 'sent' OR event_id IS NOT NULL OR synced),
  CHECK (NOT synced OR state = 'sent'),
  -- Sends taken back before they went out are deleted.
  CHECK (wanted OR state IN ('sending', 'sent'))
);

INSERT INTO matrix_send_new
  (id, room, content, queued_at, wanted, desired_by, state, error, event_id, synced)
SELECT id, room, content, queued_at, wanted, desired_by, state, error, event_id, synced
FROM matrix_send;

-- Ids are never reused, those of sends deleted included: log entries
-- refer to them. Renaming the table renames its sequence.
DELETE FROM sqlite_sequence WHERE name = 'matrix_send_new';
INSERT INTO sqlite_sequence (name, seq)
SELECT 'matrix_send_new', seq FROM sqlite_sequence WHERE name = 'matrix_send';
DROP TABLE matrix_send;
ALTER TABLE matrix_send_new RENAME TO matrix_send;
CREATE INDEX matrix_send_unreconciled ON matrix_send(room) WHERE NOT synced OR NOT wanted;
