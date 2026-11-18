-- Invitations to rooms (SPEC §9.5), of the stripped m.room.member event
-- of the account that sync gives, which has no event id, nor time: kept
-- while the room is invited to. Numbered in the order stored, so seen by
-- a watermark of their own (§12.15); news unless the account's first sync
-- stored them.
CREATE TABLE matrix_invitation (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  room         INTEGER NOT NULL UNIQUE REFERENCES matrix_room(id),
  inviter      TEXT NOT NULL,        -- mxid
  inviter_name TEXT,                 -- display name in the room, if set
  stored_at    INTEGER NOT NULL,     -- unix ms
  news         INTEGER NOT NULL CHECK (news IN (0, 1))
);

-- What was stored before a place was seen is seen: none, of invitations.
ALTER TABLE seen ADD COLUMN invitation INTEGER NOT NULL DEFAULT 0;
