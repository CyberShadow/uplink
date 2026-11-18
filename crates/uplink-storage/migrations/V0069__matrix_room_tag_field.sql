-- A Matrix room's tags reconciled as fields (SPEC §6.6): each row holds
-- the tag's `uplink_tx_log::reconcile::Field<bool>` as JSON (`field`),
-- with the columns queries need generated from it. A room's
-- `tags_known_at` is the field of every tag it has no row of: neither
-- had nor asked for, as known since then. Every tag is known as of V0066's
-- `tags_set_by`, the entry that recorded the homeserver last setting one
-- of the room's: the last push applied.

CREATE TABLE matrix_room_tag_field (
  room       INTEGER NOT NULL REFERENCES matrix_room(id) ON DELETE CASCADE,
  tag        TEXT NOT NULL,
  field      TEXT NOT NULL CHECK (json_valid(field)),
  -- Shown.
  desired    INTEGER GENERATED ALWAYS AS (json_extract(field, '$.desired')) VIRTUAL,
  -- The op asking, while pending.
  desired_by INTEGER GENERATED ALWAYS AS (json_extract(field, '$.opinion')) VIRTUAL,
  -- Whether, and why, it waits for the user's decision.
  decision   TEXT GENERATED ALWAYS AS (json_extract(field, '$.decision')) VIRTUAL,
  PRIMARY KEY (room, tag)
);

INSERT INTO matrix_room_tag_field (room, tag, field)
SELECT t.room, t.tag, json_object(
    'desired', json(CASE WHEN t.desired THEN 'true' ELSE 'false' END),
    'opinion', t.desired_by,
    'observed', json(CASE WHEN t.observed THEN 'true' ELSE 'false' END),
    'known_at', coalesce(r.tags_set_by, 0),
    'attempt', NULL,
    'straggler', NULL,
    'check', NULL,
    'decision', CASE WHEN t.refused IS NULL THEN NULL ELSE json_object('refused', t.refused) END)
FROM matrix_room_tag t JOIN matrix_room r ON r.id = t.room;

DROP TABLE matrix_room_tag;
ALTER TABLE matrix_room_tag_field RENAME TO matrix_room_tag;
CREATE INDEX matrix_room_tag_pending ON matrix_room_tag(room) WHERE desired_by IS NOT NULL;

ALTER TABLE matrix_room ADD COLUMN tags_known_at INTEGER NOT NULL DEFAULT 0;
UPDATE matrix_room SET tags_known_at = tags_set_by WHERE tags_set_by IS NOT NULL;
ALTER TABLE matrix_room DROP COLUMN tags_set_by;
