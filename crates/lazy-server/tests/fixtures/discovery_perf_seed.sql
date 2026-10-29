\set ON_ERROR_STOP on
\timing on
-- One workspace with 20,000 people and 5,000 public channels (one in 25 archived) and 50 private
-- ones the measuring person is not in; 100,000 messages over the first 300 conversations; and
-- 50,000 saved messages for the person "perfme", a sixth of them in private channels they cannot read.
INSERT INTO workspaces (id, name, event_head) VALUES ('01a00000-0000-7000-8000-0000000000a0', 'Perf', 1000000);
INSERT INTO users (id, username, display_name, password_hash)
VALUES ('01a00000-0000-7000-8000-000000000001', 'perfme', 'Perf Me', 'x');
INSERT INTO users (id, username, display_name, password_hash)
SELECT gen_random_uuid(), 'u' || lpad(n::text, 5, '0'), 'Person ' || lpad(((n * 7919) % 20000)::text, 5, '0'), 'x'
FROM generate_series(1, 20000) n;
INSERT INTO workspace_members (workspace_id, user_id, role, joined_at)
SELECT '01a00000-0000-7000-8000-0000000000a0', id, CASE WHEN username = 'perfme' THEN 'owner' ELSE 'member' END,
       now() - (random() * interval '700 days')
FROM users;

CREATE TEMP TABLE conv AS
SELECT n, gen_random_uuid() AS id,
       CASE WHEN n <= 5000 THEN 'chan' || lpad(n::text, 4, '0') ELSE 'priv' || lpad((n - 5000)::text, 3, '0') END AS name,
       n <= 5000 AS public
FROM generate_series(1, 5050) n;
INSERT INTO conversations (id, workspace_id, kind, name, topic, description, visibility, archived, is_default, member_count)
SELECT id, '01a00000-0000-7000-8000-0000000000a0', 'channel', name,
       'topic ' || (n % 50), CASE WHEN n % 3 = 0 THEN 'about thing ' || (n % 97) ELSE NULL END,
       CASE WHEN public THEN 'public' ELSE 'private' END, public AND n % 25 = 0, n = 1, (n * 37) % 500
FROM conv;
INSERT INTO conversation_members (conversation_id, user_id)
SELECT id, '01a00000-0000-7000-8000-000000000001' FROM conv WHERE n <= 50;

-- Messages: conversations 1..250 are public, 5001..5050 private (never readable by perfme).
CREATE TEMP TABLE target AS
SELECT row_number() OVER (ORDER BY n) AS k, id FROM conv WHERE n <= 250 OR n > 5000;
INSERT INTO messages (id, conversation_id, author_id, document, plain_text, seq, created_at, deleted, listed)
SELECT gen_random_uuid(), t.id, '01a00000-0000-7000-8000-000000000001',
       '{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"saved text"}]}]}'::jsonb,
       CASE WHEN s.n % 100 = 7 THEN '' ELSE 'saved text' END, s.n,
       now() - interval '200 days' + s.n * interval '1 minute', s.n % 100 = 7, s.n % 100 <> 7
FROM generate_series(1, 100000) s(n) JOIN target t ON t.k = 1 + s.n % 300;
INSERT INTO saved_messages (user_id, message_id, saved_at)
SELECT '01a00000-0000-7000-8000-000000000001', id, now() - interval '100 days' + seq * interval '1 minute'
FROM messages WHERE seq % 2 = 0 ORDER BY seq;
ANALYZE;
