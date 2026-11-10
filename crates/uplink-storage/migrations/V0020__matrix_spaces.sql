-- Spaces and avatars (SPEC §9.5): whether a room is a space, its avatar,
-- and the rooms a space lists as its children.

ALTER TABLE matrix_room ADD COLUMN space INTEGER NOT NULL DEFAULT 0;
-- mxc://server/media: the room's avatar, or of a direct chat without one,
-- the other user's.
ALTER TABLE matrix_room ADD COLUMN avatar TEXT;

-- A space's m.space.child events that list a room (with `via`).
CREATE TABLE matrix_space_child (
  space        INTEGER NOT NULL REFERENCES matrix_room(id),
  child        TEXT NOT NULL,        -- !opaque:server, joined or not
  child_order  TEXT,                 -- `order`, if valid
  PRIMARY KEY (space, child)
);

-- Rooms synced before had neither recorded: syncing afresh records every
-- room again (events already stored are not stored twice).
UPDATE account SET sync_state = NULL WHERE kind = 'matrix';
