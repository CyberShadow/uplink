-- Of a room, the latest message of a sender in the main timeline (the
-- account's own, which reads what is before it, SPEC §9.5): found by one
-- look-up as unread messages are counted, not by a scan of the room's.
-- Of just what that asks, so that no other query (of a room's latest
-- message, by position) takes it for its own.
CREATE INDEX matrix_event_messages_by_sender ON matrix_event(room, sender, pos)
  WHERE type IN ('m.room.message', 'm.room.encrypted') AND rel_type IS NOT 'm.thread';
