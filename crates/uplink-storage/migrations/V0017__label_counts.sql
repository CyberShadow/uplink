-- Each label's conversations, and unread ones, kept as entries and their
-- labels change rather than counted when asked: counting took time in
-- proportion to the mailbox. The triggers keep them, whatever writes.
--
-- What counts (SPEC §4): a conversation (an entry's thread, or the entry
-- alone) has the label if one of its entries carries it, as desired, and is
-- not in Trash or Spam (unless the label is one of those); it is unread
-- if one such entry is unread too.

ALTER TABLE label ADD COLUMN conversations INTEGER NOT NULL DEFAULT 0;
ALTER TABLE label ADD COLUMN unread_conversations INTEGER NOT NULL DEFAULT 0;

-- Each entry's labels as they count, by the rule above, now.
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

-- Each entry's labels as last counted: what to take away when it changes.
CREATE TABLE counted_label (
  entry        INTEGER NOT NULL,
  label        INTEGER NOT NULL,
  conversation TEXT NOT NULL,
  unread       INTEGER NOT NULL,
  PRIMARY KEY (entry, label)
) WITHOUT ROWID;
CREATE INDEX counted_label_by_label ON counted_label(label);

-- Per label and conversation, its entries counted, and of those unread.
-- A row exists while `entries` > 0.
CREATE TABLE label_conversation (
  label        INTEGER NOT NULL,
  conversation TEXT NOT NULL,
  entries      INTEGER NOT NULL,
  unread       INTEGER NOT NULL,
  PRIMARY KEY (label, conversation)
) WITHOUT ROWID;

-- Entries to count again. Inserting one does it (and removes it).
CREATE TABLE entry_recount (entry INTEGER NOT NULL);

CREATE TRIGGER recount_entry AFTER INSERT ON entry_recount BEGIN
  -- Take the entry away as last counted: from its labels' counts where it
  -- was its conversation's last (unread) entry for the label, ...
  UPDATE label SET unread_conversations = unread_conversations - 1
  WHERE id IN (SELECT c.label FROM counted_label c
               JOIN label_conversation lc ON lc.label = c.label AND lc.conversation = c.conversation
               WHERE c.entry = NEW.entry AND c.unread AND lc.unread = 1);
  UPDATE label SET conversations = conversations - 1
  WHERE id IN (SELECT c.label FROM counted_label c
               JOIN label_conversation lc ON lc.label = c.label AND lc.conversation = c.conversation
               WHERE c.entry = NEW.entry AND lc.entries = 1);
  -- ... and from its conversations'.
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

  -- Count it as it is now, the same way round.
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

-- What changes how an entry counts. (An upsert that updates fires the
-- update triggers, not the insert ones.)
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
CREATE TRIGGER mail_entry_removed AFTER DELETE ON mail_entry BEGIN
  INSERT INTO entry_recount VALUES (OLD.id);
END;
CREATE TRIGGER mail_entry_rethreaded AFTER UPDATE OF thread_id ON mail_entry BEGIN
  INSERT INTO entry_recount VALUES (NEW.id);
END;
-- A label's removal takes its entries' rows with it (and recounts them,
-- by the cascade); what counted it goes too.
CREATE TRIGGER label_removed AFTER DELETE ON label BEGIN
  DELETE FROM counted_label WHERE label = OLD.id;
  DELETE FROM label_conversation WHERE label = OLD.id;
END;

-- The mailboxes as they are.
INSERT INTO counted_label (entry, label, conversation, unread)
SELECT entry, label, conversation, unread FROM entry_label_counted;
INSERT INTO label_conversation (label, conversation, entries, unread)
SELECT label, conversation, count(*), sum(unread) FROM counted_label
GROUP BY label, conversation;
UPDATE label SET
  conversations = (SELECT count(*) FROM label_conversation WHERE label = label.id),
  unread_conversations = (SELECT count(*) FROM label_conversation
                          WHERE label = label.id AND unread > 0);
