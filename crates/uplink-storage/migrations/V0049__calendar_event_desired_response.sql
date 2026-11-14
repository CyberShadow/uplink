-- The user's answer to a meeting request as desired state (SPEC §6.6,
-- §10.3): `response` stays as the provider has it, `desired_response`
-- is the answer asked for while it differs, made by the op
-- `response_by`, or refused by the provider (`response_refused`, why),
-- which waits for a decision.
ALTER TABLE calendar_event ADD COLUMN desired_response TEXT;
ALTER TABLE calendar_event ADD COLUMN response_by INTEGER
  REFERENCES tx_log(id) DEFERRABLE INITIALLY DEFERRED
  CHECK ((desired_response IS NULL) = (response_by IS NULL));
ALTER TABLE calendar_event ADD COLUMN response_refused TEXT
  CHECK (response_refused IS NULL OR desired_response IS NOT NULL);
CREATE INDEX calendar_event_pending ON calendar_event(account_id) WHERE desired_response IS NOT NULL;
