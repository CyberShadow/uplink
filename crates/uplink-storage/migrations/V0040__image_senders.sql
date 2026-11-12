-- Senders whose mail's remote images are shown unasked (SPEC §8.9), by
-- address, lowercased.

CREATE TABLE image_sender (
  address TEXT PRIMARY KEY NOT NULL
) WITHOUT ROWID;
