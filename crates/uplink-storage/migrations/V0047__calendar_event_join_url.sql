-- Where to join an event's online meeting (SPEC §10.1), so that the
-- calendar can join it from a list: Teams' (`onlineMeeting.joinUrl`),
-- else another provider's (`onlineMeetingUrl`); from the JSON kept.
ALTER TABLE calendar_event ADD COLUMN join_url TEXT;
UPDATE calendar_event SET
  join_url = coalesce(json_extract(raw, '$.onlineMeeting.joinUrl'), json_extract(raw, '$.onlineMeetingUrl'));
