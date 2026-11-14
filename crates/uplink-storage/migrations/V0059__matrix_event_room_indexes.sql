-- Of a room, its latest event's time, which orders the rooms list, and
-- its mentions since a marker, which a room without its fully-read
-- marker has all of its events after: found by these, not by every
-- event of the room, at each list of the rooms.
CREATE INDEX matrix_event_by_ts ON matrix_event(room, ts);
CREATE INDEX matrix_event_highlights ON matrix_event(room, pos) WHERE highlight;
