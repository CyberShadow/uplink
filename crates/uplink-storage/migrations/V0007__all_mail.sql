-- All Mail lists an account's entries newest first (SPEC §8.1).
CREATE INDEX mail_entry_by_account_received ON mail_entry(account_id, received_at DESC, id DESC);
