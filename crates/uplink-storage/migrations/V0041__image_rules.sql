-- What has a mail's remote images shown unasked (SPEC §8.9): no longer
-- only its sender's address, but rules: of the sender's address, of the
-- sender's domain, or of an image's host; lowercased.

CREATE TABLE image_rule (
  kind  TEXT NOT NULL CHECK (kind IN ('sender', 'domain', 'host')),
  value TEXT NOT NULL,
  PRIMARY KEY (kind, value)
) WITHOUT ROWID;

INSERT INTO image_rule (kind, value) SELECT 'sender', address FROM image_sender;
DROP TABLE image_sender;

-- Recorded ops are undone, and events replayed, by their JSON: theirs
-- take the new shape, a sender's address becoming a sender rule.
UPDATE tx_log
SET op_json = json_object(
  'rule', json_object('type', 'sender', 'address', json_extract(op_json, '$.sender')),
  'always', json(iif(json_extract(op_json, '$.always'), 'true', 'false')))
WHERE op_type = 'mail.always_show_images' AND json_type(op_json, '$.sender') IS NOT NULL;

UPDATE tx_log
SET inverse_json = json_object(
  'rule', json_object('type', 'sender', 'address', json_extract(inverse_json, '$.sender')),
  'always', json(iif(json_extract(inverse_json, '$.always'), 'true', 'false')))
WHERE inverse_type = 'mail.always_show_images'
  AND json_type(inverse_json, '$.sender') IS NOT NULL;

UPDATE tx_log
SET events_json = (
  SELECT json_group_array(
    CASE
      WHEN json_extract(e.value, '$.type') = 'image_sender_changed'
      THEN json_object(
        'type', 'image_rule_changed',
        'rule', json_object('type', 'sender', 'address', json_extract(e.value, '$.sender')),
        'always', json(iif(json_extract(e.value, '$.always'), 'true', 'false')))
      ELSE json(e.value)
    END)
  FROM json_each(tx_log.events_json) e
)
WHERE EXISTS (
  SELECT 1 FROM json_each(tx_log.events_json) e
  WHERE json_extract(e.value, '$.type') = 'image_sender_changed'
);
