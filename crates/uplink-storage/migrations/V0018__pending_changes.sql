-- What is not yet reconciled with the provider, in the user's terms (SPEC
-- §6.6): the op that made each pending label change, and the provider's
-- refusal of it; and why an account's changes are not reaching it.

-- The op whose entry set `desired`, while it differs from `observed`.
-- Deferred: an op sets it before its own entry is recorded. Changes made
-- before this was recorded have none.
ALTER TABLE entry_label ADD COLUMN desired_by INTEGER
  REFERENCES tx_log(id) DEFERRABLE INITIALLY DEFERRED;
-- The provider's reason for refusing the change, which is then not pushed
-- again until desired changes.
ALTER TABLE entry_label ADD COLUMN refused TEXT;

-- While the account's task is failing (§8.8): the error, and since when
-- (unix ms).
ALTER TABLE account ADD COLUMN push_error TEXT;
ALTER TABLE account ADD COLUMN push_failing_since INTEGER;
