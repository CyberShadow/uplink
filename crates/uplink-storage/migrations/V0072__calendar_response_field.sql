-- The user's answer to a meeting request reconciled as a field (SPEC
-- §6.6, §10.3): the event's `uplink_tx_log::reconcile::Field<EventResponse>`
-- as JSON, with the columns queries need generated from it. Every event
-- is stored with its field, so there is no default.

ALTER TABLE calendar_event ADD COLUMN response_field TEXT NOT NULL DEFAULT '{}'
  CHECK (json_valid(response_field));

-- An answer asked for is pending as it was.
UPDATE calendar_event SET response_field = json_object(
    'desired', coalesce(desired_response, response),
    'opinion', response_by,
    'observed', response,
    'known_at', 0,
    'attempt', NULL,
    'straggler', NULL,
    'check', NULL,
    'not_kept', 0,
    'decision', CASE WHEN response_refused IS NOT NULL
                     THEN json_object('refused', response_refused) END);

DROP INDEX calendar_event_pending;
ALTER TABLE calendar_event DROP COLUMN response_by;
ALTER TABLE calendar_event DROP COLUMN response_refused;
ALTER TABLE calendar_event DROP COLUMN desired_response;
ALTER TABLE calendar_event DROP COLUMN response;

-- As the provider has it.
ALTER TABLE calendar_event ADD COLUMN response TEXT
  GENERATED ALWAYS AS (json_extract(response_field, '$.observed')) VIRTUAL;
-- Shown: as asked for, else as had.
ALTER TABLE calendar_event ADD COLUMN desired_response TEXT
  GENERATED ALWAYS AS (json_extract(response_field, '$.desired')) VIRTUAL;
-- The op asking, while pending.
ALTER TABLE calendar_event ADD COLUMN response_by INTEGER
  GENERATED ALWAYS AS (json_extract(response_field, '$.opinion')) VIRTUAL;
-- Whether, and why, it waits for the user's decision.
ALTER TABLE calendar_event ADD COLUMN response_decision TEXT
  GENERATED ALWAYS AS (json_extract(response_field, '$.decision')) VIRTUAL;
CREATE INDEX calendar_event_pending ON calendar_event(account_id) WHERE response_by IS NOT NULL;
