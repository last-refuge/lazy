-- What the threads release (migration 0007, protocol 6) added to the workspace in
-- populated_0005.sql and populated_0006.sql: a thread under alice's old message (one reply that
-- stayed in its thread, one also sent to the channel), the summary, and each person's state,
-- written in the shapes that release produced. Apply after migration 0007.
--
--   r1 ...00f6 bob "thread only"   r2 ...00f7 carol "also in the channel"   (both under m1 ...00f1)

INSERT INTO messages (id, conversation_id, author_id, document, plain_text, revision, seq, created_at,
                      thread_root_id, in_channel, listed) VALUES
    ('01a00000-0000-7000-8000-0000000000f6', '01a00000-0000-7000-8000-0000000000c1',
     '01a00000-0000-7000-8000-000000000002',
     '{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"thread only"}]}]}',
     'thread only', 1, 13, '2026-10-01T10:20:00Z',
     '01a00000-0000-7000-8000-0000000000f1', false, false),
    ('01a00000-0000-7000-8000-0000000000f7', '01a00000-0000-7000-8000-0000000000c1',
     '01a00000-0000-7000-8000-000000000003',
     '{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"also in the channel"}]}]}',
     'also in the channel', 1, 15, '2026-10-01T10:21:00Z',
     '01a00000-0000-7000-8000-0000000000f1', true, true);

INSERT INTO threads (root_id, conversation_id, reply_count, last_reply_seq, last_reply_at, recent_repliers, as_of) VALUES
    ('01a00000-0000-7000-8000-0000000000f1', '01a00000-0000-7000-8000-0000000000c1', 2, 15,
     '2026-10-01T10:21:00Z',
     ARRAY['01a00000-0000-7000-8000-000000000003', '01a00000-0000-7000-8000-000000000002']::uuid[], 16);

-- Alice started the thread and has read none of it; bob replied first; carol replied and read to hers.
INSERT INTO thread_user_state (user_id, root_id, conversation_id, following, unfollowed, last_read) VALUES
    ('01a00000-0000-7000-8000-000000000001', '01a00000-0000-7000-8000-0000000000f1',
     '01a00000-0000-7000-8000-0000000000c1', true, false, 0),
    ('01a00000-0000-7000-8000-000000000002', '01a00000-0000-7000-8000-0000000000f1',
     '01a00000-0000-7000-8000-0000000000c1', true, false, 13),
    ('01a00000-0000-7000-8000-000000000003', '01a00000-0000-7000-8000-0000000000f1',
     '01a00000-0000-7000-8000-0000000000c1', true, false, 15);

INSERT INTO events (workspace_id, seq, occurred_at, kind, conversation_id, operation_id, payload,
                    audience, message_id) VALUES
    ('01a00000-0000-7000-8000-0000000000a0', 13, '2026-10-01T10:20:00Z', 'message_created',
     '01a00000-0000-7000-8000-0000000000c1', NULL,
     '{"type":"message_created","operation_id":null,"message":{"id":"01a00000-0000-7000-8000-0000000000f6","seq":13,"revision":1,"conversation_id":"01a00000-0000-7000-8000-0000000000c1","author_id":"01a00000-0000-7000-8000-000000000002","created_at":"2026-10-01T10:20:00Z","thread_root":"01a00000-0000-7000-8000-0000000000f1","document":{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"thread only"}]}]}}}',
     'members', '01a00000-0000-7000-8000-0000000000f6'),
    ('01a00000-0000-7000-8000-0000000000a0', 14, '2026-10-01T10:20:00Z', 'thread_changed',
     '01a00000-0000-7000-8000-0000000000c1', NULL,
     '{"type":"thread_changed","conversation_id":"01a00000-0000-7000-8000-0000000000c1","root_id":"01a00000-0000-7000-8000-0000000000f1","summary":{"reply_count":1,"last_reply_seq":13,"last_reply_at":"2026-10-01T10:20:00Z","recent_repliers":["01a00000-0000-7000-8000-000000000002"],"as_of":14}}',
     'members', NULL),
    ('01a00000-0000-7000-8000-0000000000a0', 15, '2026-10-01T10:21:00Z', 'message_created',
     '01a00000-0000-7000-8000-0000000000c1', NULL,
     '{"type":"message_created","operation_id":null,"message":{"id":"01a00000-0000-7000-8000-0000000000f7","seq":15,"revision":1,"conversation_id":"01a00000-0000-7000-8000-0000000000c1","author_id":"01a00000-0000-7000-8000-000000000003","created_at":"2026-10-01T10:21:00Z","thread_root":"01a00000-0000-7000-8000-0000000000f1","in_channel":true,"document":{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"also in the channel"}]}]}}}',
     'members', '01a00000-0000-7000-8000-0000000000f7'),
    ('01a00000-0000-7000-8000-0000000000a0', 16, '2026-10-01T10:21:00Z', 'thread_changed',
     '01a00000-0000-7000-8000-0000000000c1', NULL,
     '{"type":"thread_changed","conversation_id":"01a00000-0000-7000-8000-0000000000c1","root_id":"01a00000-0000-7000-8000-0000000000f1","summary":{"reply_count":2,"last_reply_seq":15,"last_reply_at":"2026-10-01T10:21:00Z","recent_repliers":["01a00000-0000-7000-8000-000000000003","01a00000-0000-7000-8000-000000000002"],"as_of":16}}',
     'members', NULL);

UPDATE workspaces SET event_head = 16 WHERE id = '01a00000-0000-7000-8000-0000000000a0';
