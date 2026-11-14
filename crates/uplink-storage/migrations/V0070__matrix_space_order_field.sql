-- A space's order reconciled as a field (SPEC §6.6): the room's
-- `uplink_tx_log::reconcile::Field<Option<String>>` as JSON, with the
-- columns queries need generated from it. A room never told of an order
-- has none, known so since the log's start.

ALTER TABLE matrix_room ADD COLUMN space_order_field TEXT NOT NULL
  DEFAULT '{"desired":null,"opinion":null,"observed":null,"known_at":0,"attempt":null,"straggler":null,"check":null,"not_kept":0,"decision":null}'
  CHECK (json_valid(space_order_field));

-- A change asked for is pending while it differs from what was had.
UPDATE matrix_room SET space_order_field = json_object(
    'desired', CASE WHEN space_order_by IS NULL THEN space_order ELSE space_order_desired END,
    'opinion', CASE WHEN space_order_desired IS NOT space_order THEN space_order_by END,
    'observed', space_order,
    'known_at', 0,
    'attempt', NULL,
    'straggler', NULL,
    'check', NULL,
    'not_kept', 0,
    'decision', CASE WHEN space_order_by IS NOT NULL AND space_order_desired IS NOT space_order
                      AND space_order_error IS NOT NULL
                     THEN json_object('refused', space_order_error) END);

ALTER TABLE matrix_room DROP COLUMN space_order;
ALTER TABLE matrix_room DROP COLUMN space_order_desired;
ALTER TABLE matrix_room DROP COLUMN space_order_by;
ALTER TABLE matrix_room DROP COLUMN space_order_error;

-- Shown.
ALTER TABLE matrix_room ADD COLUMN space_order_desired TEXT
  GENERATED ALWAYS AS (json_extract(space_order_field, '$.desired')) VIRTUAL;
-- The op asking, while pending.
ALTER TABLE matrix_room ADD COLUMN space_order_by INTEGER
  GENERATED ALWAYS AS (json_extract(space_order_field, '$.opinion')) VIRTUAL;
-- Whether, and why, it waits for the user's decision.
ALTER TABLE matrix_room ADD COLUMN space_order_decision TEXT
  GENERATED ALWAYS AS (json_extract(space_order_field, '$.decision')) VIRTUAL;
