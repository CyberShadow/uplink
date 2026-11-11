-- A Matrix message of text is Markdown, or else plain text sent as it is
-- (`/plain`, SPEC §9.3): its content says which. The messages so far were
-- Markdown, as are those the log would send again by undo or redo.

UPDATE matrix_send SET content = json_set(content, '$.markdown', json('true'))
WHERE json_extract(content, '$.type') = 'text';

UPDATE tx_log SET op_json = json_set(op_json, '$.content.markdown', json('true'))
WHERE op_type = 'matrix.send' AND json_extract(op_json, '$.content.type') = 'text';
UPDATE tx_log SET inverse_json = json_set(inverse_json, '$.content.markdown', json('true'))
WHERE inverse_type = 'matrix.send' AND json_extract(inverse_json, '$.content.type') = 'text';
