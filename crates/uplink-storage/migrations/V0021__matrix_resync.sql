-- The first syncs since V0020 recorded no rooms where the SDK skipped a
-- response ending where it had synced to (SPEC §9.2): sync afresh again,
-- now that it does not.
UPDATE account SET sync_state = NULL WHERE kind = 'matrix';
