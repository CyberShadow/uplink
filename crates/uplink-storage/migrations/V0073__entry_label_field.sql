-- Mail labels reconciled by the model (SPEC §6.6, §8): each entry_label
-- row is an `uplink_tx_log::reconcile::Field<bool>`, stored as columns,
-- as the hottest table should be. `desired`, `observed` and `desired_by`
-- (its opinion) stay; `decision` replaces `refused`, as a `Decision`'s
-- JSON; `push` holds the rest (the attempt, a straggler, a read-back
-- asked for), NULL when there is none.
-- `known_at` is the account's (`account.labels_known_at`): one task per
-- account observes and pushes in turn, so the requests it sends are after
-- all it has recorded.
--
-- The table is rebuilt, as its CHECK changes: a row may be neither
-- desired nor observed while a push of it awaits its outcome. The view
-- and triggers counting labels (V0017) are made again as they were.

ALTER TABLE account ADD COLUMN labels_known_at INTEGER NOT NULL DEFAULT 0;

CREATE TABLE entry_label_field (
  entry      INTEGER NOT NULL REFERENCES mail_entry(id) ON DELETE CASCADE,
  label      INTEGER NOT NULL REFERENCES label(id) ON DELETE CASCADE,
  desired    INTEGER NOT NULL,
  observed   INTEGER NOT NULL,
  desired_by INTEGER REFERENCES tx_log(id) DEFERRABLE INITIALLY DEFERRED,
  decision   TEXT CHECK (decision IS NULL OR json_valid(decision)),
  push       TEXT CHECK (push IS NULL OR json_valid(push)),
  PRIMARY KEY (entry, label),
  CHECK (desired OR observed OR desired_by IS NOT NULL OR push IS NOT NULL),
  -- The model's: an opinion while desired differs, or a decision waits.
  CHECK (desired_by IS NOT NULL OR (desired = observed AND decision IS NULL))
);

-- Changes made before ops were recorded with them (V0018) have none: the
-- last entry asked for them, as far as anything tells.
INSERT INTO entry_label_field (entry, label, desired, observed, desired_by, decision)
SELECT entry, label, desired, observed,
       CASE WHEN desired != observed
            THEN coalesce(desired_by, (SELECT max(id) FROM tx_log)) END,
       CASE WHEN desired != observed AND refused IS NOT NULL
            THEN json_object('refused', refused) END
FROM entry_label;

DROP TRIGGER recount_entry;
DROP VIEW entry_label_counted;
DROP TABLE entry_label;
ALTER TABLE entry_label_field RENAME TO entry_label;

CREATE INDEX entry_label_by_label ON entry_label(label, entry) WHERE desired;
-- What the reconciler has to do with.
CREATE INDEX entry_label_reconciling ON entry_label(entry)
  WHERE desired_by IS NOT NULL OR push IS NOT NULL;

-- As V0017.
CREATE VIEW entry_label_counted AS
SELECT el.entry, el.label,
       coalesce(e.thread_id, '#' || e.id) AS conversation,
       EXISTS (SELECT 1 FROM entry_label u JOIN label ul ON ul.id = u.label
               WHERE u.entry = el.entry AND u.desired AND ul.external_id = 'UNREAD') AS unread
FROM entry_label el
JOIN mail_entry e ON e.id = el.entry
JOIN label l ON l.id = el.label
WHERE el.desired
  AND (l.external_id IN ('TRASH', 'SPAM')
       OR NOT EXISTS (SELECT 1 FROM entry_label h JOIN label hl ON hl.id = h.label
                      WHERE h.entry = el.entry AND h.desired
                        AND hl.external_id IN ('TRASH', 'SPAM')));

CREATE TRIGGER recount_entry AFTER INSERT ON entry_recount BEGIN
  UPDATE label SET unread_conversations = unread_conversations - 1
  WHERE id IN (SELECT c.label FROM counted_label c
               JOIN label_conversation lc ON lc.label = c.label AND lc.conversation = c.conversation
               WHERE c.entry = NEW.entry AND c.unread AND lc.unread = 1);
  UPDATE label SET conversations = conversations - 1
  WHERE id IN (SELECT c.label FROM counted_label c
               JOIN label_conversation lc ON lc.label = c.label AND lc.conversation = c.conversation
               WHERE c.entry = NEW.entry AND lc.entries = 1);
  UPDATE label_conversation
  SET entries = entries - 1, unread = label_conversation.unread - c.unread
  FROM counted_label c
  WHERE c.entry = NEW.entry
    AND c.label = label_conversation.label AND c.conversation = label_conversation.conversation;
  DELETE FROM label_conversation
  WHERE entries = 0
    AND (label, conversation) IN (SELECT label, conversation FROM counted_label
                                  WHERE entry = NEW.entry);
  DELETE FROM counted_label WHERE entry = NEW.entry;

  INSERT INTO counted_label (entry, label, conversation, unread)
  SELECT entry, label, conversation, unread FROM entry_label_counted WHERE entry = NEW.entry;
  INSERT INTO label_conversation (label, conversation, entries, unread)
  SELECT label, conversation, 1, unread FROM counted_label WHERE entry = NEW.entry
  ON CONFLICT (label, conversation)
  DO UPDATE SET entries = entries + 1, unread = label_conversation.unread + excluded.unread;
  UPDATE label SET conversations = conversations + 1
  WHERE id IN (SELECT c.label FROM counted_label c
               JOIN label_conversation lc ON lc.label = c.label AND lc.conversation = c.conversation
               WHERE c.entry = NEW.entry AND lc.entries = 1);
  UPDATE label SET unread_conversations = unread_conversations + 1
  WHERE id IN (SELECT c.label FROM counted_label c
               JOIN label_conversation lc ON lc.label = c.label AND lc.conversation = c.conversation
               WHERE c.entry = NEW.entry AND c.unread AND lc.unread = 1);

  DELETE FROM entry_recount WHERE entry = NEW.entry;
END;

CREATE TRIGGER entry_label_added AFTER INSERT ON entry_label BEGIN
  INSERT INTO entry_recount VALUES (NEW.entry);
END;
CREATE TRIGGER entry_label_desired AFTER UPDATE OF desired ON entry_label
WHEN OLD.desired IS NOT NEW.desired BEGIN
  INSERT INTO entry_recount VALUES (NEW.entry);
END;
CREATE TRIGGER entry_label_removed AFTER DELETE ON entry_label BEGIN
  INSERT INTO entry_recount VALUES (OLD.entry);
END;
