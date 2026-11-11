-- Where the user puts a space among the top-level spaces (MSC3230: the
-- `order` of its `org.matrix.msc3230.space_order` room account data), as
-- desired state (SPEC §6.6, §9.5).

-- As the homeserver has it, if valid.
ALTER TABLE matrix_room ADD COLUMN space_order TEXT;
-- Asked for (NULL: no order), by the op `space_order_by`; set iff a
-- change is pending.
ALTER TABLE matrix_room ADD COLUMN space_order_desired TEXT;
ALTER TABLE matrix_room ADD COLUMN space_order_by INTEGER
  REFERENCES tx_log(id) DEFERRABLE INITIALLY DEFERRED;
-- Why the homeserver refused the change asked for.
ALTER TABLE matrix_room ADD COLUMN space_order_error TEXT;

-- Rooms synced before have no order recorded: syncing afresh records
-- every room again (events already stored are not stored twice).
UPDATE account SET sync_state = NULL WHERE kind = 'matrix';
