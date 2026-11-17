-- Of a room, the entry that last recorded the homeserver setting one of
-- its tags (SPEC §9.5): a sync requested before it may tell of the tags
-- before, which then are not taken for the homeserver's.
ALTER TABLE matrix_room ADD COLUMN tags_set_by INTEGER;
