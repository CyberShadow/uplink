-- Where events are in their room's timeline (SPEC §9.2), apart from
-- their ids, which are in the order they were stored: events backfilled
-- are older than those stored before them. A synced event comes 2^20
-- after the room's last; events backfilled into a gap come just before
-- the event after it, one apart, so that positions never change.
ALTER TABLE matrix_event ADD COLUMN pos INTEGER NOT NULL DEFAULT 0;
UPDATE matrix_event SET pos = id * 1048576;
CREATE INDEX matrix_event_by_pos ON matrix_event(room, pos);

-- Events missing from a room's timeline, just before a stored event: of
-- a sync that did not bring every event since the last (`limited`), and
-- before the first stored, unless it is the room's creation.
CREATE TABLE matrix_gap (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  room         INTEGER NOT NULL REFERENCES matrix_room(id),
  before_event INTEGER NOT NULL UNIQUE REFERENCES matrix_event(id)
);
CREATE INDEX matrix_gap_by_room ON matrix_gap(room);

INSERT INTO matrix_gap (room, before_event)
SELECT e.room, e.id FROM matrix_event e
WHERE e.id = (SELECT min(f.id) FROM matrix_event f WHERE f.room = e.room)
  AND e.type != 'm.room.create';
