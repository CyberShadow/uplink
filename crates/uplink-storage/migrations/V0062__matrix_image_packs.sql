-- Image packs (MSC2545, SPEC §9.9): rooms' packs (`im.ponies.room_emotes`,
-- by state key) and the account's own (`im.ponies.user_emotes`), with
-- their images usable as emoji; and the rooms' packs the account uses
-- everywhere (`im.ponies.emote_rooms`).

CREATE TABLE matrix_image_pack (
  id         INTEGER PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES account(id),
  room       INTEGER REFERENCES matrix_room(id),  -- NULL: the account's own
  state_key  TEXT NOT NULL,                       -- '' of the account's own
  name       TEXT,                                -- `pack.display_name`
  UNIQUE (room, state_key)
);
CREATE INDEX matrix_image_pack_account ON matrix_image_pack (account_id);

CREATE TABLE matrix_pack_image (
  pack       INTEGER NOT NULL REFERENCES matrix_image_pack(id) ON DELETE CASCADE,
  shortcode  TEXT NOT NULL,
  url        TEXT NOT NULL,                       -- mxc://server/media
  body       TEXT,
  PRIMARY KEY (pack, shortcode)
);
CREATE INDEX matrix_pack_image_url ON matrix_pack_image (url);

CREATE TABLE matrix_emote_room (
  account_id TEXT NOT NULL REFERENCES account(id),
  room_id    TEXT NOT NULL,                       -- !opaque:server, joined or not
  state_key  TEXT NOT NULL,
  PRIMARY KEY (account_id, room_id, state_key)
);

-- Rooms synced before have no packs recorded: syncing afresh records
-- every room again (events already stored are not stored twice).
UPDATE account SET sync_state = NULL WHERE kind = 'matrix';
