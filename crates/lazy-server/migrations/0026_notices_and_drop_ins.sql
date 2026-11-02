-- Notices and live drop-ins (llm-docs/DECISIONS.md, D44). Additive.

-- What a notice the server posted says happened (lazy_domain::Notice); NULL for every other
-- message. The document holds the same in words.
ALTER TABLE messages ADD COLUMN notice jsonb;
-- A notice that does not count as unread (joins, leaves, changes to a channel; not a drop-in).
ALTER TABLE messages ADD COLUMN quiet boolean NOT NULL DEFAULT false;

-- Unread counts stay index-only: the history index carries the flag.
DROP INDEX messages_history_idx;
CREATE INDEX messages_history_idx
    ON messages (conversation_id, seq DESC) INCLUDE (author_id, deleted, quiet)
    WHERE listed;

-- The drop-in going on in a conversation, as clients are shown it (lazy_protocol::LiveDropIn);
-- NULL when none is. Kept in step with the live row of drop_ins.
ALTER TABLE conversations ADD COLUMN drop_in jsonb;

-- Drop-ins, as the call server reports them (rtc_proto::webhook). One live (not ended) per
-- conversation.
CREATE TABLE drop_ins (
    id uuid PRIMARY KEY,
    conversation_id uuid NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    -- Its notice. Not a foreign key: retention may remove the message while the row stays.
    message_id uuid NOT NULL,
    started_by uuid NOT NULL REFERENCES users (id),
    started_at timestamptz NOT NULL,
    ended_at timestamptz,
    -- Who is in it now, and everyone who has been, in the order they joined.
    participants uuid[] NOT NULL DEFAULT '{}',
    joined uuid[] NOT NULL DEFAULT '{}',
    -- The call server's time on the newest report applied (milliseconds since the epoch): an older
    -- one arriving late is ignored.
    reported_at_ms bigint NOT NULL,
    -- When a report about it last arrived: a live drop-in not heard of for a while has ended.
    heard_at timestamptz NOT NULL
);
CREATE UNIQUE INDEX drop_ins_live_idx ON drop_ins (conversation_id) WHERE ended_at IS NULL;
CREATE INDEX drop_ins_heard_idx ON drop_ins (heard_at) WHERE ended_at IS NULL;
