-- A Matrix send sends more than plain text (SPEC §9.3): its content is
-- the protocol's `MatrixContent`, as JSON: a message (in Markdown, maybe
-- an emote, a reply or an edit), a reaction, a redaction or a file. The
-- sends so far were plain-text messages.

CREATE TABLE matrix_send_new (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  room        INTEGER NOT NULL REFERENCES matrix_room(id),
  content     TEXT NOT NULL CHECK (json_valid(content)),
  queued_at   INTEGER NOT NULL,  -- unix ms
  wanted      INTEGER NOT NULL,  -- desired: 0 once cancelled
  desired_by  INTEGER NOT NULL REFERENCES tx_log(id) DEFERRABLE INITIALLY DEFERRED,
  state       TEXT NOT NULL CHECK (state IN ('queued', 'sending', 'sent', 'refused')),
  error       TEXT,              -- why the homeserver refused it
  event_id    TEXT,              -- once sent
  synced      INTEGER NOT NULL,  -- its event is synced
  CHECK ((state = 'refused') = (error IS NOT NULL)),
  CHECK ((state = 'sent') = (event_id IS NOT NULL)),
  CHECK (NOT synced OR state = 'sent'),
  -- Sends taken back before they went out are deleted.
  CHECK (wanted OR state IN ('sending', 'sent'))
);

INSERT INTO matrix_send_new
  (id, room, content, queued_at, wanted, desired_by, state, error, event_id, synced)
SELECT id, room,
       json_object('type', 'text', 'body', body, 'emote', json('false'),
                   'reply_to', NULL, 'replaces', NULL),
       queued_at, wanted, desired_by, state, error, event_id, synced
FROM matrix_send;

-- Ids are never reused, those of sends deleted included: log entries
-- refer to them. Renaming the table renames its sequence.
DELETE FROM sqlite_sequence WHERE name = 'matrix_send_new';
INSERT INTO sqlite_sequence (name, seq)
SELECT 'matrix_send_new', seq FROM sqlite_sequence WHERE name = 'matrix_send';
DROP TABLE matrix_send;
ALTER TABLE matrix_send_new RENAME TO matrix_send;
CREATE INDEX matrix_send_unreconciled ON matrix_send(room) WHERE NOT synced OR NOT wanted;

-- Ops are undone and redone from the log, so the recorded `matrix.send`
-- ops (and the inverses that are one) take the new shape.
UPDATE tx_log SET op_json = json_object(
  'room', json_extract(op_json, '$.room'),
  'content', json_object('type', 'text', 'body', json_extract(op_json, '$.body'),
                         'emote', json('false'), 'reply_to', NULL, 'replaces', NULL))
WHERE op_type = 'matrix.send';
UPDATE tx_log SET inverse_json = json_object(
  'room', json_extract(inverse_json, '$.room'),
  'content', json_object('type', 'text', 'body', json_extract(inverse_json, '$.body'),
                         'emote', json('false'), 'reply_to', NULL, 'replaces', NULL))
WHERE inverse_type = 'matrix.send';
