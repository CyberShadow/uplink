-- While the provider's watch of the account is failing, so that its mail
-- is only polled (SPEC §8.14): the error, and since when (unix ms).
ALTER TABLE account ADD COLUMN watch_error TEXT;
ALTER TABLE account ADD COLUMN watch_failing_since INTEGER;
