-- What Matrix events relate to (SPEC §9.2), read from their JSON as they
-- are stored, so that a timeline folds edits, reactions and redactions
-- into the events they are of, and shows replies with what they answer:
--
--   rel_type    `m.relates_to.rel_type`: m.replace, m.annotation, m.thread, …
--   relates_to  `m.relates_to.event_id`: the event it is of
--   reply_to    `m.relates_to.m.in_reply_to.event_id`
--   redacts     of a redaction, the event it redacts (`redacts`, in the
--               content from room version 11, at the top level before)
--   redacted    whether the event is redacted: it came so (with
--               `unsigned.redacted_because`), or a redaction was synced
--               since, which redacted the stored JSON too. A redacted
--               event keeps what it related to, so that it stays folded.

ALTER TABLE matrix_event ADD COLUMN rel_type TEXT;
ALTER TABLE matrix_event ADD COLUMN relates_to TEXT;
ALTER TABLE matrix_event ADD COLUMN reply_to TEXT;
ALTER TABLE matrix_event ADD COLUMN redacts TEXT;
ALTER TABLE matrix_event ADD COLUMN redacted INTEGER NOT NULL DEFAULT 0;

UPDATE matrix_event SET
  rel_type = json_extract(raw_json, '$.content."m.relates_to".rel_type'),
  relates_to = json_extract(raw_json, '$.content."m.relates_to".event_id'),
  reply_to = json_extract(raw_json, '$.content."m.relates_to"."m.in_reply_to".event_id'),
  redacts = CASE WHEN type = 'm.room.redaction'
                 THEN coalesce(json_extract(raw_json, '$.content.redacts'),
                               json_extract(raw_json, '$.redacts')) END,
  redacted = json_type(raw_json, '$.unsigned.redacted_because') IS NOT NULL;

CREATE INDEX matrix_event_relations ON matrix_event(room, relates_to) WHERE relates_to IS NOT NULL;
CREATE INDEX matrix_event_replies ON matrix_event(room, reply_to) WHERE reply_to IS NOT NULL;
CREATE INDEX matrix_event_redactions ON matrix_event(room, redacts) WHERE redacts IS NOT NULL;

-- Redactions synced before this migration did not redact what they
-- redact: they do so when their room next syncs (`redacted` is set then).
