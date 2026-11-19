-- A to-do flagged from a room's message (SPEC §11.2), as `message` is of
-- one flagged from mail: the event is flagged while the to-do is open.
-- Event rows are never deleted, so the link holds. A to-do is of one or
-- the other, or of neither.
ALTER TABLE todo ADD COLUMN room_event INTEGER REFERENCES matrix_event(id);
CREATE INDEX todo_by_room_event ON todo(room_event) WHERE room_event IS NOT NULL;
