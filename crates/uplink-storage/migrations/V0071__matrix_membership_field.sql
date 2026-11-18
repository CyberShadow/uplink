-- A room's membership reconciled as a field (SPEC §6.6, §9.7): the
-- room's `uplink_tx_log::reconcile::Field<Membership>` as JSON, with the
-- columns queries need generated from it. Every room is stored with its
-- field, so there is no default.

ALTER TABLE matrix_room ADD COLUMN membership_field TEXT NOT NULL DEFAULT '{}'
  CHECK (json_valid(membership_field));

-- A change asked for is pending while it differs from what was had.
UPDATE matrix_room SET membership_field = json_object(
    'desired', coalesce(desired, membership),
    'opinion', CASE WHEN desired IS NOT membership THEN membership_by END,
    'observed', membership,
    'known_at', 0,
    'attempt', NULL,
    'straggler', NULL,
    'check', NULL,
    'decision', CASE WHEN desired IS NOT membership AND membership_error IS NOT NULL
                     THEN json_object('refused', membership_error) END);

ALTER TABLE matrix_room DROP COLUMN membership;
ALTER TABLE matrix_room DROP COLUMN desired;
ALTER TABLE matrix_room DROP COLUMN membership_by;
ALTER TABLE matrix_room DROP COLUMN membership_error;

-- As the homeserver has it: 'joined', 'invited' or 'left'.
ALTER TABLE matrix_room ADD COLUMN membership TEXT
  GENERATED ALWAYS AS (json_extract(membership_field, '$.observed')) VIRTUAL;
-- Asked for, while pending.
ALTER TABLE matrix_room ADD COLUMN desired TEXT GENERATED ALWAYS AS (
  CASE WHEN json_extract(membership_field, '$.opinion') IS NOT NULL
       THEN json_extract(membership_field, '$.desired') END) VIRTUAL;
-- The op asking, while pending.
ALTER TABLE matrix_room ADD COLUMN membership_by INTEGER
  GENERATED ALWAYS AS (json_extract(membership_field, '$.opinion')) VIRTUAL;
-- Whether, and why, it waits for the user's decision.
ALTER TABLE matrix_room ADD COLUMN membership_decision TEXT
  GENERATED ALWAYS AS (json_extract(membership_field, '$.decision')) VIRTUAL;
