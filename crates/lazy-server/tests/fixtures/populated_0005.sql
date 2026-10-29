-- A workspace as server version 0005 (protocol 4) left it: people, a default channel with an
-- edited message, a reacted-to message, a deleted one (events already rewritten to the tombstone),
-- a receipt, and a session. Written by hand in the shapes the 0005 server produced.
--
--   alice (owner) 01a00000-0000-7000-8000-000000000001, token lzy_ + 64 x 'a'
--   bob           ...0002        carol ...0003
--   workspace     ...00a0        #general ...00c1 (default)   #random ...00c2
--   m1 ...00f1 alice "hello", edited once, bob reacted 👍   m2 ...00f2 bob, deleted   m3 ...00f3 carol

INSERT INTO users (id, username, display_name, password_hash) VALUES
    ('01a00000-0000-7000-8000-000000000001', 'alice', 'Alice', 'x'),
    ('01a00000-0000-7000-8000-000000000002', 'bob', 'Bob', 'x'),
    ('01a00000-0000-7000-8000-000000000003', 'carol', 'Carol', 'x');

INSERT INTO sessions (id, user_id, token_hash, expires_at) VALUES
    ('01a00000-0000-7000-8000-0000000000e1', '01a00000-0000-7000-8000-000000000001',
     sha256(convert_to('lzy_' || repeat('a', 64), 'UTF8')), now() + interval '30 days'),
    ('01a00000-0000-7000-8000-0000000000e2', '01a00000-0000-7000-8000-000000000002',
     sha256(convert_to('lzy_' || repeat('b', 64), 'UTF8')), now() + interval '30 days'),
    ('01a00000-0000-7000-8000-0000000000e3', '01a00000-0000-7000-8000-000000000003',
     sha256(convert_to('lzy_' || repeat('c', 64), 'UTF8')), now() + interval '30 days');

INSERT INTO workspaces (id, name, event_head) VALUES
    ('01a00000-0000-7000-8000-0000000000a0', 'Lazy', 9);

INSERT INTO workspace_members (workspace_id, user_id, role) VALUES
    ('01a00000-0000-7000-8000-0000000000a0', '01a00000-0000-7000-8000-000000000001', 'owner'),
    ('01a00000-0000-7000-8000-0000000000a0', '01a00000-0000-7000-8000-000000000002', 'member'),
    ('01a00000-0000-7000-8000-0000000000a0', '01a00000-0000-7000-8000-000000000003', 'member');

INSERT INTO conversations (id, workspace_id, kind, name, topic, visibility, is_default, member_count) VALUES
    ('01a00000-0000-7000-8000-0000000000c1', '01a00000-0000-7000-8000-0000000000a0',
     'channel', 'general', 'Where everyone starts', 'public', true, 3),
    ('01a00000-0000-7000-8000-0000000000c2', '01a00000-0000-7000-8000-0000000000a0',
     'channel', 'random', NULL, 'public', false, 2);

INSERT INTO conversation_members (conversation_id, user_id) VALUES
    ('01a00000-0000-7000-8000-0000000000c1', '01a00000-0000-7000-8000-000000000001'),
    ('01a00000-0000-7000-8000-0000000000c1', '01a00000-0000-7000-8000-000000000002'),
    ('01a00000-0000-7000-8000-0000000000c1', '01a00000-0000-7000-8000-000000000003'),
    ('01a00000-0000-7000-8000-0000000000c2', '01a00000-0000-7000-8000-000000000001'),
    ('01a00000-0000-7000-8000-0000000000c2', '01a00000-0000-7000-8000-000000000002');

-- Alice and Carol have read everything; Bob has read nothing.
INSERT INTO conversation_user_state (user_id, conversation_id, last_read) VALUES
    ('01a00000-0000-7000-8000-000000000001', '01a00000-0000-7000-8000-0000000000c1', 9),
    ('01a00000-0000-7000-8000-000000000002', '01a00000-0000-7000-8000-0000000000c1', 0),
    ('01a00000-0000-7000-8000-000000000003', '01a00000-0000-7000-8000-0000000000c1', 9),
    ('01a00000-0000-7000-8000-000000000001', '01a00000-0000-7000-8000-0000000000c2', 0),
    ('01a00000-0000-7000-8000-000000000002', '01a00000-0000-7000-8000-0000000000c2', 0);

INSERT INTO messages (id, conversation_id, author_id, document, plain_text, revision, seq, created_at,
                      edited_at, deleted, reactions_as_of) VALUES
    ('01a00000-0000-7000-8000-0000000000f1', '01a00000-0000-7000-8000-0000000000c1',
     '01a00000-0000-7000-8000-000000000001',
     '{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"hello (edited)"}]}]}',
     'hello (edited)', 2, 4, '2026-10-01T10:00:00Z', '2026-10-01T10:01:00Z', false, 6),
    ('01a00000-0000-7000-8000-0000000000f2', '01a00000-0000-7000-8000-0000000000c1',
     '01a00000-0000-7000-8000-000000000002',
     '{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"This message was deleted."}]}]}',
     '', 2, 7, '2026-10-01T10:03:00Z', NULL, true, 8),
    ('01a00000-0000-7000-8000-0000000000f3', '01a00000-0000-7000-8000-0000000000c1',
     '01a00000-0000-7000-8000-000000000003',
     '{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"plain message"}]}]}',
     'plain message', 1, 9, '2026-10-01T10:05:00Z', NULL, false, 0);

INSERT INTO message_revisions (message_id, revision, author_id, document, edited_at) VALUES
    ('01a00000-0000-7000-8000-0000000000f1', 1, '01a00000-0000-7000-8000-000000000001',
     '{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"hello"}]}]}', NULL);

INSERT INTO message_reactions (message_id, user_id, emoji, seq) VALUES
    ('01a00000-0000-7000-8000-0000000000f1', '01a00000-0000-7000-8000-000000000002', '👍', 6);

INSERT INTO operation_receipts (user_id, operation_id, request_hash, message_id, kind) VALUES
    ('01a00000-0000-7000-8000-000000000001', '01a00000-0000-7000-8000-0000000000d1',
     '\x00', '01a00000-0000-7000-8000-0000000000f1', 'send_message');

INSERT INTO events (workspace_id, seq, occurred_at, kind, conversation_id, operation_id, payload,
                    audience, message_id) VALUES
    ('01a00000-0000-7000-8000-0000000000a0', 1, '2026-10-01T09:00:00Z', 'member_joined', NULL, NULL,
     '{"type":"member_joined","user":{"id":"01a00000-0000-7000-8000-000000000001","username":"alice","display_name":"Alice"}}',
     'workspace', NULL),
    ('01a00000-0000-7000-8000-0000000000a0', 2, '2026-10-01T09:01:00Z', 'member_joined', NULL, NULL,
     '{"type":"member_joined","user":{"id":"01a00000-0000-7000-8000-000000000002","username":"bob","display_name":"Bob"}}',
     'workspace', NULL),
    ('01a00000-0000-7000-8000-0000000000a0', 3, '2026-10-01T09:02:00Z', 'member_joined', NULL, NULL,
     '{"type":"member_joined","user":{"id":"01a00000-0000-7000-8000-000000000003","username":"carol","display_name":"Carol"}}',
     'workspace', NULL),
    ('01a00000-0000-7000-8000-0000000000a0', 4, '2026-10-01T10:00:00Z', 'message_created',
     '01a00000-0000-7000-8000-0000000000c1', '01a00000-0000-7000-8000-0000000000d1',
     '{"type":"message_created","operation_id":"01a00000-0000-7000-8000-0000000000d1","message":{"id":"01a00000-0000-7000-8000-0000000000f1","seq":4,"revision":1,"conversation_id":"01a00000-0000-7000-8000-0000000000c1","author_id":"01a00000-0000-7000-8000-000000000001","created_at":"2026-10-01T10:00:00Z","document":{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"hello"}]}]}}}',
     'members', '01a00000-0000-7000-8000-0000000000f1'),
    ('01a00000-0000-7000-8000-0000000000a0', 5, '2026-10-01T10:01:00Z', 'message_edited',
     '01a00000-0000-7000-8000-0000000000c1', NULL,
     '{"type":"message_edited","operation_id":null,"message":{"id":"01a00000-0000-7000-8000-0000000000f1","seq":4,"revision":2,"conversation_id":"01a00000-0000-7000-8000-0000000000c1","author_id":"01a00000-0000-7000-8000-000000000001","created_at":"2026-10-01T10:00:00Z","edited_at":"2026-10-01T10:01:00Z","document":{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"hello (edited)"}]}]}}}',
     'members', '01a00000-0000-7000-8000-0000000000f1'),
    ('01a00000-0000-7000-8000-0000000000a0', 6, '2026-10-01T10:02:00Z', 'reactions_changed',
     '01a00000-0000-7000-8000-0000000000c1', NULL,
     '{"type":"reactions_changed","conversation_id":"01a00000-0000-7000-8000-0000000000c1","message_id":"01a00000-0000-7000-8000-0000000000f1","reactions":[{"emoji":"👍","users":["01a00000-0000-7000-8000-000000000002"]}]}',
     'members', NULL),
    ('01a00000-0000-7000-8000-0000000000a0', 7, '2026-10-01T10:03:00Z', 'message_created',
     '01a00000-0000-7000-8000-0000000000c1', NULL,
     '{"type":"message_created","operation_id":null,"message":{"id":"01a00000-0000-7000-8000-0000000000f2","seq":7,"revision":2,"deleted":true,"reactions_as_of":8,"conversation_id":"01a00000-0000-7000-8000-0000000000c1","author_id":"01a00000-0000-7000-8000-000000000002","created_at":"2026-10-01T10:03:00Z","document":{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"This message was deleted."}]}]}}}',
     'members', '01a00000-0000-7000-8000-0000000000f2'),
    ('01a00000-0000-7000-8000-0000000000a0', 8, '2026-10-01T10:04:00Z', 'message_deleted',
     '01a00000-0000-7000-8000-0000000000c1', NULL,
     '{"type":"message_deleted","message":{"id":"01a00000-0000-7000-8000-0000000000f2","seq":7,"revision":2,"deleted":true,"reactions_as_of":8,"conversation_id":"01a00000-0000-7000-8000-0000000000c1","author_id":"01a00000-0000-7000-8000-000000000002","created_at":"2026-10-01T10:03:00Z","document":{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"This message was deleted."}]}]}}}',
     'members', '01a00000-0000-7000-8000-0000000000f2'),
    ('01a00000-0000-7000-8000-0000000000a0', 9, '2026-10-01T10:05:00Z', 'message_created',
     '01a00000-0000-7000-8000-0000000000c1', NULL,
     '{"type":"message_created","operation_id":null,"message":{"id":"01a00000-0000-7000-8000-0000000000f3","seq":9,"revision":1,"conversation_id":"01a00000-0000-7000-8000-0000000000c1","author_id":"01a00000-0000-7000-8000-000000000003","created_at":"2026-10-01T10:05:00Z","document":{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"plain message"}]}]}}}',
     'members', '01a00000-0000-7000-8000-0000000000f3');
