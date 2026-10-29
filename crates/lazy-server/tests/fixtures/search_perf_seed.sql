\set ON_ERROR_STOP on
\timing on
-- One workspace, 300 people, 200 channels (40 public, 160 private), 1,000,000 messages.
INSERT INTO workspaces (id, name, event_head) VALUES ('01a00000-0000-7000-8000-0000000000a0', 'Perf', 1000000);
INSERT INTO users (id, username, display_name, password_hash)
SELECT gen_random_uuid(), 'u' || lpad(n::text, 4, '0'), 'User ' || n, 'x' FROM generate_series(1, 300) n;
INSERT INTO users (id, username, display_name, password_hash) VALUES
    ('01a00000-0000-7000-8000-000000000001', 'perfme', 'Perf Me', 'x'),
    ('01a00000-0000-7000-8000-000000000002', 'newbie', 'New Member', 'x');
INSERT INTO workspace_members (workspace_id, user_id, role)
SELECT '01a00000-0000-7000-8000-0000000000a0', id, 'member' FROM users;

CREATE TEMP TABLE conv AS
SELECT n, gen_random_uuid() AS id, CASE WHEN n <= 40 THEN 'public' ELSE 'private' END AS vis
FROM generate_series(1, 200) n;
INSERT INTO conversations (id, workspace_id, kind, name, visibility, is_default, member_count)
SELECT id, '01a00000-0000-7000-8000-0000000000a0', 'channel', 'chan' || lpad(n::text, 3, '0'), vis, n = 1, 0 FROM conv;
-- perfme: 10 public + 150 private. newbie: only #chan001. Everyone else: about 15 conversations.
INSERT INTO conversation_members (conversation_id, user_id)
SELECT c.id, '01a00000-0000-7000-8000-000000000001' FROM conv c WHERE c.n <= 10 OR (c.n > 40 AND c.n <= 190);
INSERT INTO conversation_members (conversation_id, user_id)
SELECT c.id, '01a00000-0000-7000-8000-000000000002' FROM conv c WHERE c.n = 1;
INSERT INTO conversation_members (conversation_id, user_id)
SELECT DISTINCT c.id, u.id FROM users u CROSS JOIN LATERAL
    (SELECT conv.id FROM conv, generate_series(1, 15) g WHERE conv.n = 1 + floor(200 * random() + 0 * g * length(u.username))::int) c
WHERE u.username LIKE 'u%'
ON CONFLICT DO NOTHING;
UPDATE conversations c SET member_count = (SELECT count(*) FROM conversation_members m WHERE m.conversation_id = c.id);

-- Vocabulary: 5,000 words, Zipf-ish by rank, real-looking.
CREATE TEMP TABLE vocab AS
SELECT n, 'word' || n AS w FROM generate_series(1, 5000) n;
CREATE TEMP TABLE vocab_arr AS SELECT array_agg(w ORDER BY n) AS a FROM vocab;

CREATE TEMP TABLE gen AS
SELECT n,
       1 + floor(200 * power(random(), 1.8))::int AS c,
       1 + floor(300 * random())::int AS u,
       6 + floor(random() * 14)::int AS len,
       random() AS r
FROM generate_series(1, 1000000) n;

CREATE TEMP TABLE texted AS
SELECT g.n, g.c, g.u,
  (SELECT string_agg((SELECT a[1 + floor(5000 * power(random(), 3) + 0 * g.n * x)::int] FROM vocab_arr), ' ')
     FROM generate_series(1, g.len) x)
  || CASE WHEN g.r < 0.60 THEN ' the' ELSE '' END
  || CASE WHEN g.r BETWEEN 0.10 AND 0.15 THEN ' deploy' ELSE '' END
  || CASE WHEN g.r BETWEEN 0.20 AND 0.22 THEN ' deployment' ELSE '' END
  || CASE WHEN g.r BETWEEN 0.30 AND 0.31 THEN ' dependency' ELSE '' END
  || CASE WHEN g.r BETWEEN 0.40 AND 0.4002 THEN ' rollback plan' ELSE '' END
  || CASE WHEN g.r BETWEEN 0.50 AND 0.5002 THEN ' rollback' ELSE '' END
  || CASE WHEN g.r BETWEEN 0.70 AND 0.7001 THEN ' kubernetes' ELSE '' END
  || CASE WHEN g.r BETWEEN 0.80 AND 0.81 THEN ' plan' ELSE '' END AS txt
FROM gen g;

INSERT INTO messages (id, conversation_id, author_id, document, plain_text, seq, created_at)
SELECT gen_random_uuid(), conv.id, usr.id,
       jsonb_build_object('version', 2, 'blocks', jsonb_build_array(jsonb_build_object('type', 'paragraph',
         'inlines', jsonb_build_array(jsonb_build_object('type', 'text', 'text', t.txt))))),
       t.txt, t.n,
       now() - interval '730 days' + t.n * (interval '730 days' / 1000000)
FROM texted t
JOIN conv ON conv.n = t.c
JOIN (SELECT id, row_number() OVER (ORDER BY username) AS rn FROM users WHERE username LIKE 'u%') usr ON usr.rn = t.u;

-- 8% of messages have a file; half pictures. Names have words, some deleted.
INSERT INTO attachments (id, workspace_id, conversation_id, uploader_id, name, media_type, size, sha256, message_id, ordinal, deleted_at)
SELECT gen_random_uuid(), '01a00000-0000-7000-8000-0000000000a0', m.conversation_id, m.author_id,
       'file_' || m.seq || '_report.' || CASE WHEN m.seq % 4 < 2 THEN 'png' WHEN m.seq % 4 = 2 THEN 'jpg' ELSE 'pdf' END,
       CASE WHEN m.seq % 4 < 2 THEN 'image/png' WHEN m.seq % 4 = 2 THEN 'image/jpeg' ELSE 'application/pdf' END,
       1000, decode(repeat('ab', 32), 'hex'), m.id, 0,
       CASE WHEN m.seq % 50 = 0 THEN now() ELSE NULL END
FROM messages m WHERE m.seq % 12 = 0;
-- 2% of messages are deleted (and have no text).
UPDATE messages SET deleted = true, plain_text = '', listed = false WHERE seq % 50 = 7;
