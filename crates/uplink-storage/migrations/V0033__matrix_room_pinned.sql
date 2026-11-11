-- The events pinned in a Matrix room (`m.room.pinned_events`), as a JSON
-- array of their ids, in the state's order, and whether the account may
-- pin and unpin them, by the room's power levels (SPEC §9.5). Rooms get
-- them when they next sync.

ALTER TABLE matrix_room ADD COLUMN pinned TEXT NOT NULL DEFAULT '[]';
ALTER TABLE matrix_room ADD COLUMN may_pin INTEGER NOT NULL DEFAULT 0;
