-- Whether a message has attachments other than images its HTML body
-- shows, for the message list. NULL until indexed: messages stored before
-- this are read from their .eml when the server starts
-- (uplink_email::mailbox::index_attachments).
ALTER TABLE message ADD COLUMN has_attachments INTEGER;
