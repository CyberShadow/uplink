-- A Matrix room's tags (`m.tag` room account data: `m.favourite`,
-- `m.lowpriority`, the user's own `u.*`), as desired state (SPEC §6.6,
-- §9.5), as entry_label is of mail: `observed` as the homeserver has it,
-- `desired` as the user asked; they differ while a change is pending,
-- made by the op `desired_by`, or refused by the homeserver (`refused`,
-- why), which waits for a decision.

CREATE TABLE matrix_room_tag (
  room       INTEGER NOT NULL REFERENCES matrix_room(id) ON DELETE CASCADE,
  tag        TEXT NOT NULL,
  desired    INTEGER NOT NULL,
  observed   INTEGER NOT NULL,
  desired_by INTEGER REFERENCES tx_log(id) DEFERRABLE INITIALLY DEFERRED,
  refused    TEXT,
  PRIMARY KEY (room, tag),
  CHECK (desired OR observed),
  CHECK ((desired != observed) = (desired_by IS NOT NULL))
);
CREATE INDEX matrix_room_tag_pending ON matrix_room_tag(room) WHERE desired != observed;

-- Rooms synced before have no tags recorded: syncing afresh records
-- every room again (events already stored are not stored twice).
UPDATE account SET sync_state = NULL WHERE kind = 'matrix';
