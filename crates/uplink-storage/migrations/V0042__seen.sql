-- When each place of the nav was last seen (SPEC §12.15): of each kind,
-- the latest id stored then, as ids are given in the order things come.
-- `place` is `home`, `todos`, `calendar`, `label:<id>`, `account:<id>`
-- (its All Mail) or `room:<id>`; All inboxes is seen as its Inboxes.
CREATE TABLE seen (
  place       TEXT PRIMARY KEY,
  mail        INTEGER NOT NULL,    -- mail_entry.id
  room_event  INTEGER NOT NULL,    -- matrix_event.id
  todo        INTEGER NOT NULL,    -- todo.id
  event       INTEGER NOT NULL     -- calendar_event.id
) WITHOUT ROWID;
