-- A Matrix send cancelled once it went out is taken back where Matrix
-- allows (SPEC §6.7, §9.3): a message, a file, a reaction or an edit is
-- redacted; a pin, reversed. `taking_back` while that is being sent;
-- `taken_back` once done, kept so that sending it again (the undo's undo)
-- knows what to send. `error` is also why the homeserver refused to take
-- one back, which needs a decision.

CREATE TABLE matrix_send_new (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  room        INTEGER NOT NULL REFERENCES matrix_room(id),
  content     TEXT NOT NULL CHECK (json_valid(content)),
  queued_at   INTEGER NOT NULL,  -- unix ms
  wanted      INTEGER NOT NULL,  -- desired: 0 once cancelled
  desired_by  INTEGER NOT NULL REFERENCES tx_log(id) DEFERRABLE INITIALLY DEFERRED,
  state       TEXT NOT NULL CHECK (state IN
                ('queued', 'sending', 'sent', 'refused', 'taking_back', 'taken_back')),
  error       TEXT,              -- why the homeserver refused it, or to take it back
  event_id    TEXT,              -- once sent, if the homeserver said
  synced      INTEGER NOT NULL,  -- its event is synced
  CHECK (state <> 'refused' OR error IS NOT NULL),
  CHECK (error IS NULL OR state = 'refused' OR (state = 'sent' AND NOT wanted)),
  CHECK (event_id IS NULL OR state IN ('sent', 'taking_back', 'taken_back')),
  CHECK (state <> 'sent' OR event_id IS NOT NULL OR synced),
  CHECK (NOT synced OR state IN ('sent', 'taking_back', 'taken_back')),
  -- Sends taken back before they went out are deleted.
  CHECK (wanted OR state IN ('sending', 'sent', 'taking_back', 'taken_back')),
  CHECK (NOT wanted OR state NOT IN ('taking_back', 'taken_back'))
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
-- Reconciled: sent and synced, as wanted; or taken back.
CREATE INDEX matrix_send_unreconciled ON matrix_send(room)
  WHERE NOT ((wanted AND synced) OR state = 'taken_back');
