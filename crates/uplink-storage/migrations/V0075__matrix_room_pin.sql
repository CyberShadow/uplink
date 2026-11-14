-- A Matrix room's pinned events reconciled as fields (SPEC §6.6.4, §9.5),
-- rather than pins and unpins sent: each row holds an event's
-- `uplink_tx_log::reconcile::Field<bool>` as JSON (`field`), with the
-- columns queries need generated from it, and the event's place in the
-- room's `m.room.pinned_events` as last observed (`pos`; NULL if not
-- there). A room's `pins_known_at` is the field of every event it has no
-- row of: neither pinned nor asked to be, as known since then.

ALTER TABLE matrix_room ADD COLUMN pins_known_at INTEGER NOT NULL DEFAULT 0;

CREATE TABLE matrix_room_pin (
  room       INTEGER NOT NULL REFERENCES matrix_room(id) ON DELETE CASCADE,
  event      TEXT NOT NULL,
  field      TEXT NOT NULL CHECK (json_valid(field)),
  pos        INTEGER,
  -- Shown.
  desired    INTEGER GENERATED ALWAYS AS (json_extract(field, '$.desired')) VIRTUAL,
  -- The op asking, while pending.
  desired_by INTEGER GENERATED ALWAYS AS (json_extract(field, '$.opinion')) VIRTUAL,
  -- Whether, and why, it waits for the user's decision.
  decision   TEXT GENERATED ALWAYS AS (json_extract(field, '$.decision')) VIRTUAL,
  PRIMARY KEY (room, event)
);
CREATE INDEX matrix_room_pin_pending ON matrix_room_pin(room) WHERE desired_by IS NOT NULL;

-- The events pinned, as the rooms last synced them.
INSERT INTO matrix_room_pin (room, event, field, pos)
SELECT r.id, p.value, json_object(
    'desired', json('true'), 'opinion', NULL, 'observed', json('true'), 'known_at', 0,
    'attempt', NULL, 'straggler', NULL, 'check', NULL, 'not_kept', 0, 'decision', NULL),
  min(p.key)
FROM matrix_room r, json_each(r.pinned) p
WHERE p.type = 'text'
GROUP BY r.id, p.value;

-- Pins and unpins sent, not yet reconciled: the latest of each event
-- asks for it pinned or not (taken back, for one no longer wanted), by
-- the op that last wanted it so; refused, it waits for a decision. One
-- asking for what is so already is done.
CREATE TEMP TABLE pin_asked AS
SELECT s.room, s.content ->> '$.event' AS event,
       CASE WHEN s.wanted THEN s.content ->> '$.pinned' ELSE NOT (s.content ->> '$.pinned') END
         AS desired,
       s.desired_by, s.error
FROM matrix_send s
WHERE s.content ->> '$.type' = 'pin'
  AND NOT ((s.wanted AND s.synced) OR s.state = 'taken_back')
  AND s.id = (SELECT max(o.id) FROM matrix_send o
              WHERE o.room = s.room AND o.content ->> '$.type' = 'pin'
                AND o.content ->> '$.event' = s.content ->> '$.event'
                AND NOT ((o.wanted AND o.synced) OR o.state = 'taken_back'));

INSERT INTO matrix_room_pin (room, event, field)
SELECT a.room, a.event, json_object(
    'desired', json('true'), 'opinion', a.desired_by, 'observed', json('false'), 'known_at', 0,
    'attempt', NULL, 'straggler', NULL, 'check', NULL, 'not_kept', 0,
    'decision', json(CASE WHEN a.error IS NULL THEN NULL ELSE json_object('refused', a.error) END))
FROM pin_asked a
WHERE a.desired
  AND NOT EXISTS (SELECT 1 FROM matrix_room_pin p WHERE p.room = a.room AND p.event = a.event);

UPDATE matrix_room_pin
SET field = json_set(field,
    '$.desired', json('false'), '$.opinion', a.desired_by,
    '$.decision', json(CASE WHEN a.error IS NULL THEN NULL ELSE json_object('refused', a.error) END))
FROM pin_asked a
WHERE NOT a.desired AND a.room = matrix_room_pin.room AND a.event = matrix_room_pin.event;

DROP TABLE pin_asked;

-- Recorded ops are undone by their JSON: those of pin sends (sending,
-- cancelling, keeping sent) become the pins they asked for, and their
-- inverses the pins undoing them. A send's pin is told by its row, or,
-- of one cancelled before it went, by the send the cancel's inverse is.
CREATE TEMP TABLE pin_item AS
SELECT id AS item, room, content ->> '$.event' AS event, content ->> '$.pinned' AS pinned
FROM matrix_send WHERE content ->> '$.type' = 'pin'
UNION
SELECT inverse_json ->> '$.item', op_json ->> '$.room',
       op_json ->> '$.content.event', op_json ->> '$.content.pinned'
FROM tx_log
WHERE op_type = 'matrix.send' AND op_json ->> '$.content.type' = 'pin'
UNION
SELECT op_json ->> '$.item', inverse_json ->> '$.room',
       inverse_json ->> '$.content.event', inverse_json ->> '$.content.pinned'
FROM tx_log
WHERE op_type = 'matrix.cancel_send' AND inverse_type = 'matrix.send'
  AND inverse_json ->> '$.content.type' = 'pin';

-- (op pins, as asked) per kind: a send and a keep pin; a cancel unpins.
CREATE TEMP TABLE pin_entry AS
SELECT t.id, i.room, i.event,
       CASE t.op_type WHEN 'matrix.cancel_send' THEN NOT i.pinned ELSE i.pinned END AS pinned
FROM tx_log t JOIN pin_item i
  ON i.item = CASE t.op_type WHEN 'matrix.send' THEN t.inverse_json ->> '$.item'
                             ELSE t.op_json ->> '$.item' END
WHERE (t.op_type = 'matrix.send' AND t.op_json ->> '$.content.type' = 'pin')
   OR t.op_type IN ('matrix.cancel_send', 'matrix.keep_sent');

UPDATE tx_log
SET op_type = 'matrix.set_pins',
    op_json = json_object('changes', json_array(json_object(
      'room', e.room, 'event', e.event, 'pinned', json(iif(e.pinned, 'true', 'false'))))),
    inverse_type = 'matrix.set_pins',
    inverse_json = json_object('changes', json_array(json_object(
      'room', e.room, 'event', e.event, 'pinned', json(iif(e.pinned, 'false', 'true')))))
FROM pin_entry e
WHERE e.id = tx_log.id;

DROP TABLE pin_entry;
DROP TABLE pin_item;

DELETE FROM matrix_send WHERE content ->> '$.type' = 'pin';
ALTER TABLE matrix_room DROP COLUMN pinned;
