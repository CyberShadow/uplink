-- When an event last changed, as Graph says (`lastModifiedDateTime`, of
-- the raw JSON kept): lists of every kind order an event by it (SPEC
-- §8.7), as mail by when it came. An event without it fails here, rather
-- than being listed as never changed.
ALTER TABLE calendar_event ADD COLUMN modified_ts INTEGER NOT NULL DEFAULT 0;
UPDATE calendar_event SET modified_ts = CAST(round(
  (julianday(json_extract(raw, '$.lastModifiedDateTime')) - 2440587.5) * 86400000) AS INTEGER);
