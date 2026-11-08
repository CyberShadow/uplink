-- The history view lists the user's ops, newest first (SPEC §6.3). Synced
-- observations far outnumber them, so they get an index of their own.
CREATE INDEX tx_log_user_ops ON tx_log(id) WHERE account_id IS NULL;
