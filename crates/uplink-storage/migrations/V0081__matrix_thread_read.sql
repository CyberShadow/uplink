-- A Matrix thread's read marker (MSC3771 threaded receipts, SPEC §9.2,
-- §12.15): how far the thread of root `root` (a Matrix event id) is read,
-- desired state as the room's `read_up_to` is, pushed as an `m.read`
-- receipt with the root as `thread_id`; and the latest such receipt sent
-- or observed (`read_pushed`). A thread's message is read once either
-- its thread's marker or the room's is past it, so what was read before
-- (at or before the room's markers) stays so: no thread needs a row.
CREATE TABLE matrix_thread_read (
  room        INTEGER NOT NULL REFERENCES matrix_room(id) ON DELETE CASCADE,
  root        TEXT NOT NULL,
  read_up_to  INTEGER REFERENCES matrix_event(id),
  read_pushed INTEGER REFERENCES matrix_event(id),
  PRIMARY KEY (room, root)
);

-- The account's own latest message of a thread, which reads what it
-- answers: found by a seek, not by the thread's messages.
CREATE INDEX matrix_event_thread_by_sender ON matrix_event(room, relates_to, sender, pos)
  WHERE rel_type = 'm.thread';
