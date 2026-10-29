-- What the previous slice (migration 0006, protocol 5) added to the 0005 workspace in
-- populated_0005.sql: a user group, a message naming a person, one using @channel, written in the
-- shapes that release produced. Apply after migration 0006, on top of populated_0005.sql.
--
--   group team ...00b1 (bob, carol)   m4 ...00f4 bob "hi @carol"   m5 ...00f5 alice "@channel standup"

INSERT INTO user_groups (id, workspace_id, handle, name, created_by) VALUES
    ('01a00000-0000-7000-8000-0000000000b1', '01a00000-0000-7000-8000-0000000000a0',
     'team', 'Team', '01a00000-0000-7000-8000-000000000001');
INSERT INTO user_group_members (group_id, user_id) VALUES
    ('01a00000-0000-7000-8000-0000000000b1', '01a00000-0000-7000-8000-000000000002'),
    ('01a00000-0000-7000-8000-0000000000b1', '01a00000-0000-7000-8000-000000000003');

INSERT INTO messages (id, conversation_id, author_id, document, plain_text, revision, seq, created_at,
                      mentions, mentions_channel) VALUES
    ('01a00000-0000-7000-8000-0000000000f4', '01a00000-0000-7000-8000-0000000000c1',
     '01a00000-0000-7000-8000-000000000002',
     '{"version":3,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"hi "},{"type":"mention","target":{"type":"user","id":"01a00000-0000-7000-8000-000000000003"},"handle":"carol"}]}]}',
     'hi @carol', 1, 11, '2026-10-01T10:10:00Z',
     '{"users":["01a00000-0000-7000-8000-000000000003"]}', false),
    ('01a00000-0000-7000-8000-0000000000f5', '01a00000-0000-7000-8000-0000000000c1',
     '01a00000-0000-7000-8000-000000000001',
     '{"version":3,"blocks":[{"type":"paragraph","inlines":[{"type":"mention","target":{"type":"channel"},"handle":"channel"},{"type":"text","text":" standup"}]}]}',
     '@channel standup', 1, 12, '2026-10-01T10:11:00Z',
     '{"channel":true}', true);

INSERT INTO message_mentions (message_id, user_id, conversation_id, seq) VALUES
    ('01a00000-0000-7000-8000-0000000000f4', '01a00000-0000-7000-8000-000000000003',
     '01a00000-0000-7000-8000-0000000000c1', 11);

INSERT INTO events (workspace_id, seq, occurred_at, kind, conversation_id, operation_id, payload,
                    audience, message_id) VALUES
    ('01a00000-0000-7000-8000-0000000000a0', 10, '2026-10-01T10:09:00Z', 'group_changed', NULL, NULL,
     '{"type":"group_changed","group":{"id":"01a00000-0000-7000-8000-0000000000b1","workspace_id":"01a00000-0000-7000-8000-0000000000a0","handle":"team","name":"Team","members":["01a00000-0000-7000-8000-000000000002","01a00000-0000-7000-8000-000000000003"]}}',
     'workspace', NULL),
    ('01a00000-0000-7000-8000-0000000000a0', 11, '2026-10-01T10:10:00Z', 'message_created',
     '01a00000-0000-7000-8000-0000000000c1', NULL,
     '{"type":"message_created","operation_id":null,"message":{"id":"01a00000-0000-7000-8000-0000000000f4","seq":11,"revision":1,"conversation_id":"01a00000-0000-7000-8000-0000000000c1","author_id":"01a00000-0000-7000-8000-000000000002","created_at":"2026-10-01T10:10:00Z","mentions":{"users":["01a00000-0000-7000-8000-000000000003"]},"document":{"version":3,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"hi "},{"type":"mention","target":{"type":"user","id":"01a00000-0000-7000-8000-000000000003"},"handle":"carol"}]}]}}}',
     'members', '01a00000-0000-7000-8000-0000000000f4'),
    ('01a00000-0000-7000-8000-0000000000a0', 12, '2026-10-01T10:11:00Z', 'message_created',
     '01a00000-0000-7000-8000-0000000000c1', NULL,
     '{"type":"message_created","operation_id":null,"message":{"id":"01a00000-0000-7000-8000-0000000000f5","seq":12,"revision":1,"conversation_id":"01a00000-0000-7000-8000-0000000000c1","author_id":"01a00000-0000-7000-8000-000000000001","created_at":"2026-10-01T10:11:00Z","mentions":{"channel":true},"document":{"version":3,"blocks":[{"type":"paragraph","inlines":[{"type":"mention","target":{"type":"channel"},"handle":"channel"},{"type":"text","text":" standup"}]}]}}}',
     'members', '01a00000-0000-7000-8000-0000000000f5');

UPDATE workspaces SET event_head = 12 WHERE id = '01a00000-0000-7000-8000-0000000000a0';
