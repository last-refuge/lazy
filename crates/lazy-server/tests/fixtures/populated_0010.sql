-- What the custom emoji release (migration 0010, protocol 9) added to the earlier fixtures: a live
-- emoji and a deleted one (rows only; the images are in the attachment store, not here), and a
-- message by carol that uses the live one. Apply after migration 0010.
--
--   emoji ...00e1 `party` (live), ...00e2 `retired` (deleted); message ...00fa (seq 21) by carol

INSERT INTO custom_emoji (id, workspace_id, name, created_by, size, sha256, width, height, deleted_at,
                          image_removed) VALUES
    ('01a00000-0000-7000-8000-0000000000e1', '01a00000-0000-7000-8000-0000000000a0', 'party',
     '01a00000-0000-7000-8000-000000000001', 10, sha256(convert_to('party', 'UTF8')), 32, 32, NULL, false),
    ('01a00000-0000-7000-8000-0000000000e2', '01a00000-0000-7000-8000-0000000000a0', 'retired',
     '01a00000-0000-7000-8000-000000000001', 10, sha256(convert_to('retired', 'UTF8')), 32, 32,
     '2026-10-01T10:45:00Z', false);

INSERT INTO messages (id, conversation_id, author_id, document, plain_text, revision, seq, created_at) VALUES
    ('01a00000-0000-7000-8000-0000000000fa', '01a00000-0000-7000-8000-0000000000c1',
     '01a00000-0000-7000-8000-000000000003',
     '{"version":4,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"launch day "},{"type":"emoji","id":"01a00000-0000-7000-8000-0000000000e1","name":"party"}]}]}',
     'launch day :party:', 1, 21, '2026-10-01T10:50:00Z');
INSERT INTO message_reactions (message_id, user_id, emoji, seq) VALUES
    ('01a00000-0000-7000-8000-0000000000fa', '01a00000-0000-7000-8000-000000000001',
     'custom:01a00000-0000-7000-8000-0000000000e1', 21);
INSERT INTO events (workspace_id, seq, occurred_at, kind, conversation_id, operation_id, payload,
                    audience, message_id) VALUES
    ('01a00000-0000-7000-8000-0000000000a0', 21, '2026-10-01T10:50:00Z', 'message_created',
     '01a00000-0000-7000-8000-0000000000c1', NULL,
     '{"type":"message_created","operation_id":null,"message":{"id":"01a00000-0000-7000-8000-0000000000fa","seq":21,"revision":1,"conversation_id":"01a00000-0000-7000-8000-0000000000c1","author_id":"01a00000-0000-7000-8000-000000000003","created_at":"2026-10-01T10:50:00Z","document":{"version":4,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"launch day "},{"type":"emoji","id":"01a00000-0000-7000-8000-0000000000e1","name":"party"}]}]}}}',
     'members', '01a00000-0000-7000-8000-0000000000fa');
UPDATE workspaces SET event_head = 21 WHERE id = '01a00000-0000-7000-8000-0000000000a0';
