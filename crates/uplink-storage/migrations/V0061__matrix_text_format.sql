-- A Matrix message of text is in a format (SPEC §9.3): Markdown, plain
-- text, or HTML (`/html`), where it was Markdown or else plain text by
-- `markdown`. The sends, and those the log would send again by undo or
-- redo, say so.

UPDATE matrix_send SET content = json_remove(
  json_set(content, '$.format',
           CASE json_extract(content, '$.markdown') WHEN 1 THEN 'markdown' ELSE 'plain' END),
  '$.markdown')
WHERE json_extract(content, '$.type') = 'text';

UPDATE tx_log SET op_json = json_remove(
  json_set(op_json, '$.content.format',
           CASE json_extract(op_json, '$.content.markdown') WHEN 1 THEN 'markdown' ELSE 'plain' END),
  '$.content.markdown')
WHERE op_type = 'matrix.send' AND json_extract(op_json, '$.content.type') = 'text';

UPDATE tx_log SET inverse_json = json_remove(
  json_set(inverse_json, '$.content.format',
           CASE json_extract(inverse_json, '$.content.markdown') WHEN 1 THEN 'markdown' ELSE 'plain' END),
  '$.content.markdown')
WHERE inverse_type = 'matrix.send' AND json_extract(inverse_json, '$.content.type') = 'text';
