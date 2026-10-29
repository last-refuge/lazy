\set ON_ERROR_STOP on
\timing on
-- One workspace, 40 public channels, 60,000 threads (a root and three replies each); the person
-- "perfme" follows a third of them (20,000), half read to the newest reply and half not read at all.
INSERT INTO workspaces (id, name, event_head) VALUES ('01a00000-0000-7000-8000-0000000000a0', 'Perf', 1000000);
INSERT INTO users (id, username, display_name, password_hash) VALUES
    ('01a00000-0000-7000-8000-000000000001', 'perfme', 'Perf Me', 'x'),
    ('01a00000-0000-7000-8000-000000000002', 'bob', 'Bob', 'x');
INSERT INTO workspace_members (workspace_id, user_id, role) VALUES
    ('01a00000-0000-7000-8000-0000000000a0', '01a00000-0000-7000-8000-000000000001', 'member'),
    ('01a00000-0000-7000-8000-0000000000a0', '01a00000-0000-7000-8000-000000000002', 'member');
CREATE TEMP TABLE conv AS SELECT n, gen_random_uuid() AS id FROM generate_series(1, 40) n;
INSERT INTO conversations (id, workspace_id, kind, name, visibility, is_default, member_count)
SELECT id, '01a00000-0000-7000-8000-0000000000a0', 'channel', 'c' || lpad(n::text, 3, '0'), 'public', n = 1, 2 FROM conv;
INSERT INTO conversation_members (conversation_id, user_id)
SELECT id, u FROM conv, (VALUES ('01a00000-0000-7000-8000-000000000001'::uuid), ('01a00000-0000-7000-8000-000000000002'::uuid)) v(u);

CREATE TEMP TABLE roots AS
SELECT s.n, gen_random_uuid() AS id, c.id AS conv FROM generate_series(1, 60000) s(n) JOIN conv c ON c.n = 1 + s.n % 40;
INSERT INTO messages (id, conversation_id, author_id, document, plain_text, seq, created_at)
SELECT id, conv, '01a00000-0000-7000-8000-000000000001',
       '{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"a root"}]}]}'::jsonb,
       'a root', n * 5, now() - interval '100 days' + n * interval '1 minute' FROM roots;
INSERT INTO messages (id, conversation_id, author_id, document, plain_text, seq, created_at, thread_root_id, listed)
SELECT gen_random_uuid(), r.conv, '01a00000-0000-7000-8000-000000000002',
       '{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"a reply"}]}]}'::jsonb,
       'a reply', r.n * 5 + k, now() - interval '100 days' + r.n * interval '1 minute', r.id, false
FROM roots r, generate_series(1, 3) k;
INSERT INTO threads (root_id, conversation_id, reply_count, last_reply_seq, last_reply_at, recent_repliers, as_of)
SELECT id, conv, 3, n * 5 + 3, now() - interval '100 days' + n * interval '1 minute',
       ARRAY['01a00000-0000-7000-8000-000000000002'::uuid], n * 5 + 3 FROM roots;
INSERT INTO thread_user_state (user_id, root_id, conversation_id, following, last_read)
SELECT '01a00000-0000-7000-8000-000000000001', id, conv, true, CASE WHEN n % 2 = 0 THEN 0 ELSE n * 5 + 3 END
FROM roots WHERE n % 3 = 0;
-- And bob follows all of them (the other side of every thread).
INSERT INTO thread_user_state (user_id, root_id, conversation_id, following, last_read)
SELECT '01a00000-0000-7000-8000-000000000002', id, conv, true, n * 5 + 3 FROM roots;
ANALYZE;
