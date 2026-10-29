-- What the attachments release (migration 0009, protocol 8) added to the workspace in the earlier
-- fixtures: a file on bob's message and one still waiting, written in the shapes that release
-- produced (the bytes themselves are in the attachment store, not here). Apply after migration 0009.
--
--   m9 ...00f9 bob "with a file" (seq 20), file ...00a1 on it; ...00a2 pending, uploaded by carol

INSERT INTO messages (id, conversation_id, author_id, document, plain_text, revision, seq, created_at) VALUES
    ('01a00000-0000-7000-8000-0000000000f9', '01a00000-0000-7000-8000-0000000000c1',
     '01a00000-0000-7000-8000-000000000002',
     '{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"with a file"}]}]}',
     'with a file', 1, 20, '2026-10-01T10:40:00Z');
INSERT INTO attachments (id, workspace_id, conversation_id, uploader_id, name, media_type, size, sha256,
                         message_id, ordinal) VALUES
    ('01a00000-0000-7000-8000-0000000000a1', '01a00000-0000-7000-8000-0000000000a0',
     '01a00000-0000-7000-8000-0000000000c1', '01a00000-0000-7000-8000-000000000002',
     'notes.txt', 'text/plain', 5, sha256(convert_to('hello', 'UTF8')),
     '01a00000-0000-7000-8000-0000000000f9', 0),
    ('01a00000-0000-7000-8000-0000000000a2', '01a00000-0000-7000-8000-0000000000a0',
     '01a00000-0000-7000-8000-0000000000c1', '01a00000-0000-7000-8000-000000000003',
     'later.txt', 'text/plain', 5, sha256(convert_to('later', 'UTF8')), NULL, NULL);
INSERT INTO events (workspace_id, seq, occurred_at, kind, conversation_id, operation_id, payload,
                    audience, message_id) VALUES
    ('01a00000-0000-7000-8000-0000000000a0', 20, '2026-10-01T10:40:00Z', 'message_created',
     '01a00000-0000-7000-8000-0000000000c1', NULL,
     '{"type":"message_created","operation_id":null,"message":{"id":"01a00000-0000-7000-8000-0000000000f9","seq":20,"revision":1,"conversation_id":"01a00000-0000-7000-8000-0000000000c1","author_id":"01a00000-0000-7000-8000-000000000002","created_at":"2026-10-01T10:40:00Z","document":{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"with a file"}]}]},"attachments":[{"id":"01a00000-0000-7000-8000-0000000000a1","name":"notes.txt","media_type":"text/plain","size":5,"sha256":"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"}]}}',
     'members', '01a00000-0000-7000-8000-0000000000f9');
UPDATE workspaces SET event_head = 20 WHERE id = '01a00000-0000-7000-8000-0000000000a0';
