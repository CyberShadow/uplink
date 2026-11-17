-- A member event's member and the name it gives them, of its JSON (and
-- so as redacted), found by the room and member: a sender who had no
-- name when they spoke is named by their next member event with one,
-- found by one look-up, not by a scan of the room's member events.
ALTER TABLE matrix_event ADD COLUMN member TEXT GENERATED ALWAYS AS
  (CASE WHEN type = 'm.room.member' THEN json_extract(raw_json, '$.state_key') END) VIRTUAL;
ALTER TABLE matrix_event ADD COLUMN member_name TEXT GENERATED ALWAYS AS
  (CASE WHEN type = 'm.room.member' THEN json_extract(raw_json, '$.content.displayname') END)
  VIRTUAL;
CREATE INDEX matrix_event_member_names ON matrix_event(room, member, pos)
  WHERE member_name IS NOT NULL;
-- The events of a sender who had no name, which a name given them later
-- names: only theirs, not the room's.
CREATE INDEX matrix_event_unnamed ON matrix_event(room, sender, pos)
  WHERE sender_name IS NULL;

-- Those stored before, named once.
UPDATE matrix_event AS e
SET sender_name = (
  SELECT m.member_name FROM matrix_event m
  WHERE m.room = e.room AND m.member = e.sender AND m.member_name IS NOT NULL
    AND m.pos > e.pos
  ORDER BY m.pos LIMIT 1)
WHERE e.sender_name IS NULL AND EXISTS (
  SELECT 1 FROM matrix_event m
  WHERE m.room = e.room AND m.member = e.sender AND m.member_name IS NOT NULL
    AND m.pos > e.pos);
