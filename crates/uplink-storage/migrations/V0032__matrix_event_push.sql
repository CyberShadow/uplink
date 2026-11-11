-- Whether a Matrix event notifies, and whether it highlights (mentions
-- the account, or a keyword), by the account's push rules as its client
-- evaluates them (SPEC §9.2): for rooms' counts of unread messages that
-- notify, and browser notifications. Events stored before are neither.

ALTER TABLE matrix_event ADD COLUMN notify INTEGER NOT NULL DEFAULT 0;
ALTER TABLE matrix_event ADD COLUMN highlight INTEGER NOT NULL DEFAULT 0;
