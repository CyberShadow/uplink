-- A membership asked for records the op that asked (SPEC §6.6, §9.7), as
-- label changes and sends do, for the outbox; set iff `desired` is.
-- A refusal now leaves it asked for, refused, until the user decides
-- (undoing the op keeps the room as it is), where it used to drop it: so
-- refusals of memberships no longer asked for were decided on already.

ALTER TABLE matrix_room ADD COLUMN membership_by INTEGER
  REFERENCES tx_log(id) DEFERRABLE INITIALLY DEFERRED;

UPDATE matrix_room
SET membership_by = (
  SELECT max(t.id) FROM tx_log t
  WHERE t.op_type IN ('matrix.change_membership', 'matrix.revert_membership')
    AND json_extract(t.op_json, '$.room') = matrix_room.id)
WHERE desired IS NOT NULL;

UPDATE matrix_room SET membership_error = NULL WHERE desired IS NULL;
