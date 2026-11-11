-- Gmail replaces the Message-ID of mail it sends (SPEC §8.2), so its copy
-- in Sent is not found by ours: Gmail sends a draft, whose id it answers
-- with, and that id says which message the copy is. `provider_draft` is
-- now Gmail's draft too; `provider_thread` is the draft's thread, where
-- the copy is looked for if the answer to sending was lost.

ALTER TABLE mail_send ADD COLUMN provider_thread TEXT; -- Gmail: its draft's thread
ALTER TABLE mail_send ADD COLUMN provider_copy TEXT;   -- Gmail: its copy's message id
