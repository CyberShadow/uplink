-- A conversation's entries in time order, so its newest entry and its
-- members are read from the conversation alone. Without the order, SQLite
-- walked all the account's entries by time instead, for each conversation.
DROP INDEX mail_entry_by_thread;
CREATE INDEX mail_entry_by_thread ON mail_entry(account_id, thread_id, received_at, id);
