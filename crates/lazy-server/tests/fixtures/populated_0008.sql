-- What the webhooks release (migration 0008, protocol 7) added to the workspace in the earlier
-- fixtures: a webhook for #general, its bot (a user with no password and no way to sign in, a
-- member of the workspace and of #general), and a message the bot posted, written in the shapes
-- that release produced. Apply after migration 0008.
--
--   webhook ...00b9 (secret: 64 times "d")   bot ...0004 "bot-0000b900" named Deploys   m8 ...00f8

INSERT INTO users (id, username, display_name, password_hash, is_bot) VALUES
    ('01a00000-0000-7000-8000-000000000004', 'bot-0000b900', 'Deploys', '!', true);
INSERT INTO workspace_members (workspace_id, user_id, role) VALUES
    ('01a00000-0000-7000-8000-0000000000a0', '01a00000-0000-7000-8000-000000000004', 'member');
INSERT INTO conversation_members (conversation_id, user_id) VALUES
    ('01a00000-0000-7000-8000-0000000000c1', '01a00000-0000-7000-8000-000000000004');
UPDATE conversations SET member_count = 4 WHERE id = '01a00000-0000-7000-8000-0000000000c1';
INSERT INTO conversation_user_state (user_id, conversation_id, last_read) VALUES
    ('01a00000-0000-7000-8000-000000000004', '01a00000-0000-7000-8000-0000000000c1', 17);

INSERT INTO webhooks (id, workspace_id, conversation_id, name, bot_user_id, secret_hash, created_by) VALUES
    ('01a00000-0000-7000-8000-0000000000b9', '01a00000-0000-7000-8000-0000000000a0',
     '01a00000-0000-7000-8000-0000000000c1', 'Deploys', '01a00000-0000-7000-8000-000000000004',
     sha256(convert_to(repeat('d', 64), 'UTF8')), '01a00000-0000-7000-8000-000000000001');

INSERT INTO messages (id, conversation_id, author_id, document, plain_text, revision, seq, created_at) VALUES
    ('01a00000-0000-7000-8000-0000000000f8', '01a00000-0000-7000-8000-0000000000c1',
     '01a00000-0000-7000-8000-000000000004',
     '{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"deployed v1"}]}]}',
     'deployed v1', 1, 19, '2026-10-01T10:30:00Z');

INSERT INTO events (workspace_id, seq, occurred_at, kind, conversation_id, operation_id, payload,
                    audience, message_id) VALUES
    ('01a00000-0000-7000-8000-0000000000a0', 17, '2026-10-01T10:29:00Z', 'member_joined', NULL, NULL,
     '{"type":"member_joined","user":{"id":"01a00000-0000-7000-8000-000000000004","username":"bot-0000b900","display_name":"Deploys","is_bot":true}}',
     'workspace', NULL),
    ('01a00000-0000-7000-8000-0000000000a0', 18, '2026-10-01T10:29:00Z', 'member_added',
     '01a00000-0000-7000-8000-0000000000c1', NULL,
     '{"type":"member_added","user_id":"01a00000-0000-7000-8000-000000000004","conversation":{"id":"01a00000-0000-7000-8000-0000000000c1","kind":"channel","name":"general","topic":"Where everyone starts","archived":false,"created_by":null,"is_default":true,"visibility":"public","description":null,"member_count":4,"participants":[],"workspace_id":"01a00000-0000-7000-8000-0000000000a0"}}',
     'members', NULL),
    ('01a00000-0000-7000-8000-0000000000a0', 19, '2026-10-01T10:30:00Z', 'message_created',
     '01a00000-0000-7000-8000-0000000000c1', NULL,
     '{"type":"message_created","operation_id":null,"message":{"id":"01a00000-0000-7000-8000-0000000000f8","seq":19,"revision":1,"conversation_id":"01a00000-0000-7000-8000-0000000000c1","author_id":"01a00000-0000-7000-8000-000000000004","created_at":"2026-10-01T10:30:00Z","document":{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"deployed v1"}]}]}}}',
     'members', '01a00000-0000-7000-8000-0000000000f8');

UPDATE workspaces SET event_head = 19 WHERE id = '01a00000-0000-7000-8000-0000000000a0';
