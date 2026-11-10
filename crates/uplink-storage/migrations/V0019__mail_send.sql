-- Sending mail is desired state like any other (SPEC §6.6, §6.7): a send
-- is wanted, or no longer (cancelled when it can't be taken back for
-- sure), and its outcome observed. The outbox no longer holds sends: it
-- lists what is not reconciled, so the table is the sends', kept once
-- the provider's copy is synced.

CREATE TABLE mail_send (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id    TEXT NOT NULL,
  draft         TEXT NOT NULL,     -- JSON: the draft as sent
  message_id    TEXT NOT NULL UNIQUE, -- generated when queued, no brackets
  delay_ms      INTEGER NOT NULL,  -- the send delay it was queued with
  not_before    INTEGER NOT NULL,  -- unix ms
  wanted        INTEGER NOT NULL,  -- desired: 0 once cancelled
  desired_by    INTEGER NOT NULL REFERENCES tx_log(id) DEFERRABLE INITIALLY DEFERRED,
  state         TEXT NOT NULL
                CHECK (state IN ('queued', 'sending', 'unknown', 'sent', 'refused')),
  attempted_at  INTEGER,           -- unix ms: when the last attempt began
  provider_draft TEXT,             -- M365: the draft it is sent as (§8.4)
  error         TEXT,              -- why it was refused, or its outcome is unknown
  message       INTEGER REFERENCES message(id), -- once sent: the local copy
  synced        INTEGER NOT NULL,  -- the provider's copy is observed
  CHECK ((state IN ('refused', 'unknown')) = (error IS NOT NULL)),
  CHECK ((state = 'sent') = (message IS NOT NULL)),
  CHECK (NOT synced OR state = 'sent'),
  -- Sends taken back before they went out are deleted.
  CHECK (wanted OR state IN ('sending', 'unknown', 'sent'))
);
CREATE INDEX mail_send_due ON mail_send(account_id, not_before) WHERE state = 'queued';
CREATE INDEX mail_send_unreconciled ON mail_send(account_id) WHERE NOT synced OR NOT wanted;

INSERT INTO mail_send
  (id, account_id, draft, message_id, delay_ms, not_before, wanted, desired_by,
   state, attempted_at, error, message, synced)
SELECT id, account_id, draft, message_id, delay_ms, not_before, 1,
       (SELECT max(t.id) FROM tx_log t
        WHERE t.op_type = 'mail.send'
          AND json_extract(t.op_json, '$.id') = json_extract(o.draft, '$.id')),
       CASE WHEN state != 'failed' THEN state
            WHEN error LIKE '%may have been sent' THEN 'unknown'
            ELSE 'refused' END,
       -- Long ago: the next sync settles it.
       CASE WHEN state = 'failed' AND error LIKE '%may have been sent' THEN 0 END,
       error, message, 0
FROM mail_outbox o;

-- Ids are never reused, those of items deleted included: log entries
-- refer to them.
UPDATE sqlite_sequence
SET seq = max(seq, coalesce((SELECT seq FROM sqlite_sequence WHERE name = 'mail_outbox'), 0))
WHERE name = 'mail_send';
INSERT INTO sqlite_sequence (name, seq)
SELECT 'mail_send', seq FROM sqlite_sequence
WHERE name = 'mail_outbox'
  AND NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'mail_send');

DROP TABLE mail_outbox;
