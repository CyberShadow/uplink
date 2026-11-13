-- What the calendar shows of an event's status (SPEC §10.2): cancelled
-- but kept, how it shows the user's time, and the user's answer; as
-- Graph has them (`isCancelled`, `showAs`, `responseStatus.response`),
-- from the JSON already kept.
ALTER TABLE calendar_event ADD COLUMN cancelled INTEGER NOT NULL DEFAULT 0;
ALTER TABLE calendar_event ADD COLUMN show_as TEXT NOT NULL DEFAULT '';
ALTER TABLE calendar_event ADD COLUMN response TEXT NOT NULL DEFAULT '';
-- Every row has them; one that did not would fail here, as NULL.
UPDATE calendar_event SET
  cancelled = json_extract(raw, '$.isCancelled'),
  show_as = json_extract(raw, '$.showAs'),
  response = json_extract(raw, '$.responseStatus.response');
