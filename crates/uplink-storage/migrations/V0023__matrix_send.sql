-- Sending a Matrix message is desired state like any other (SPEC §6.6,
-- §6.7, §9.3), as mail's is (V0019): a send is wanted, or no longer
-- (cancelled while it can't be taken back for sure), and its outcome
-- observed. The table is the sends', kept once their events are synced.

CREATE TABLE matrix_send (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  room        INTEGER NOT NULL REFERENCES matrix_room(id),
  body        TEXT NOT NULL,
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
CREATE INDEX matrix_send_unreconciled ON matrix_send(room) WHERE NOT synced OR NOT wanted;

INSERT INTO matrix_send
  (id, room, body, queued_at, wanted, desired_by, state, error, event_id, synced)
SELECT id, room, body, queued_at, 1,
       -- The op that queued it: its event names it.
       (SELECT max(t.id) FROM tx_log t, json_each(t.events_json) e, json_each(e.value, '$.items') i
        WHERE t.op_type = 'matrix.send'
          AND json_extract(e.value, '$.type') = 'matrix_outbox_changed'
          AND i.value = o.id),
       CASE state WHEN 'failed' THEN 'refused' ELSE state END,
       error, event_id, 0
FROM matrix_outbox o;

-- Ids are never reused, those of sends deleted included: log entries
-- refer to them.
UPDATE sqlite_sequence
SET seq = max(seq, coalesce((SELECT seq FROM sqlite_sequence WHERE name = 'matrix_outbox'), 0))
WHERE name = 'matrix_send';
INSERT INTO sqlite_sequence (name, seq)
SELECT 'matrix_send', seq FROM sqlite_sequence
WHERE name = 'matrix_outbox'
  AND NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'matrix_send');

DROP TABLE matrix_outbox;

-- Recorded events are replayed, so they take the new shape (SPEC §5.4):
-- `matrix_sends_changed` names the rooms of the sends still kept.
UPDATE tx_log
SET events_json = (
  SELECT json_group_array(
    CASE
      WHEN json_extract(e.value, '$.type') = 'matrix_outbox_changed'
      THEN json_object(
        'type', 'matrix_sends_changed',
        'rooms', (SELECT json_group_array(DISTINCT s.room) FROM matrix_send s
                  WHERE s.id IN (SELECT i.value FROM json_each(e.value, '$.items') i)))
      ELSE json(e.value)
    END)
  FROM json_each(tx_log.events_json) e
)
WHERE EXISTS (
  SELECT 1 FROM json_each(tx_log.events_json) e
  WHERE json_extract(e.value, '$.type') = 'matrix_outbox_changed'
);
