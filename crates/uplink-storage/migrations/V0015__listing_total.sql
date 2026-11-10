-- While a mail account's first listing runs, how many messages the
-- provider says the mailbox holds (Gmail's profile), for its progress.
ALTER TABLE account ADD COLUMN listing_total INTEGER;
