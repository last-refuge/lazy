-- Threads (protocol version 6). Additive: existing databases upgrade in place, and every existing
-- message is a top-level message with no replies.

-- ---------------------------------------------------------------------------------------------
-- Replies are messages: the same row, with the root they belong to.
-- ---------------------------------------------------------------------------------------------

ALTER TABLE messages
    -- The root of the thread a reply belongs to; NULL for every top-level message. Threads are
    -- flat, so a root is never itself a reply (the server checks; a CHECK cannot see other rows).
    ADD COLUMN thread_root_id uuid REFERENCES messages (id) ON DELETE CASCADE,
    -- A reply that was also sent to the channel: the conversation's history lists it.
    ADD COLUMN in_channel     boolean NOT NULL DEFAULT false,
    -- The conversation's history lists this message: a top-level message that exists, a deleted
    -- root that still has replies (a tombstone), or an in-channel reply that exists. Unread and
    -- mention counts of a conversation consider only these (a tombstone never counts). Kept in step
    -- by the transactions that create and delete messages.
    ADD COLUMN listed         boolean NOT NULL DEFAULT true;

-- Messages deleted before this migration are gone from history.
UPDATE messages SET listed = false WHERE deleted;

ALTER TABLE messages
    ADD CONSTRAINT messages_in_channel_is_a_reply CHECK (NOT in_channel OR thread_root_id IS NOT NULL),
    ADD CONSTRAINT messages_listed_shape CHECK (
        -- A reply is listed only if it was sent to the channel and still exists...
        (NOT listed OR thread_root_id IS NULL OR (in_channel AND NOT deleted))
        -- ...and anything not listed is deleted or a reply that stayed in its thread.
        AND (listed OR deleted OR (thread_root_id IS NOT NULL AND NOT in_channel))
    );

-- The replies of a root by position: the thread's pages and its unread and mention counts, which
-- read the author and the `@channel` flag from the index and never visit the table to count.
CREATE INDEX messages_replies_idx
    ON messages (thread_root_id, seq) INCLUDE (id, author_id, mentions_channel)
    WHERE thread_root_id IS NOT NULL AND NOT deleted;

-- What a conversation's history lists, newest first: its pages, and (index-only, through the
-- columns it carries) its unread counts. Replaces nothing: `messages_conversation_seq_idx` still
-- orders every message, replies included, for the positions that need that.
CREATE INDEX messages_history_idx
    ON messages (conversation_id, seq DESC) INCLUDE (author_id, deleted)
    WHERE listed;

-- `@channel` counts for a conversation only when its history lists the message.
DROP INDEX messages_channel_mentions_idx;
CREATE INDEX messages_channel_mentions_idx
    ON messages (conversation_id, seq) INCLUDE (author_id, id)
    WHERE mentions_channel AND NOT deleted AND listed;

-- Named mentions likewise: a mention in a thread-only reply counts for its thread, not for the
-- conversation. The flag is copied from the message when the row is written and never changes
-- (a message that exists keeps its place in history, and its rows go when it is deleted).
ALTER TABLE message_mentions ADD COLUMN listed boolean NOT NULL DEFAULT true;
DROP INDEX message_mentions_user_idx;
CREATE INDEX message_mentions_user_idx
    ON message_mentions (user_id, conversation_id, seq) INCLUDE (message_id)
    WHERE listed;

-- ---------------------------------------------------------------------------------------------
-- The summary a root carries: kept in the transaction that adds or deletes a reply.
-- ---------------------------------------------------------------------------------------------

-- One row per root that has replies; deleted when the last reply is deleted. A separate table, so
-- a busy thread does not rewrite its root's (large) row on every reply.
CREATE TABLE threads (
    root_id         uuid PRIMARY KEY REFERENCES messages (id) ON DELETE CASCADE,
    conversation_id uuid        NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    -- Replies that have not been deleted.
    reply_count     integer     NOT NULL CHECK (reply_count >= 1),
    last_reply_seq  bigint      NOT NULL CHECK (last_reply_seq >= 1),
    last_reply_at   timestamptz NOT NULL,
    -- Up to five different people who replied, most recent first.
    recent_repliers uuid[]      NOT NULL CHECK (cardinality(recent_repliers) BETWEEN 1 AND 5),
    -- The position of the latest change to the thread (a reply added or deleted).
    as_of           bigint      NOT NULL CHECK (as_of >= 1)
);

-- ---------------------------------------------------------------------------------------------
-- A person's own state for a thread.
-- ---------------------------------------------------------------------------------------------

-- Rows exist for people who followed or unfollowed a thread (by choice or because a reply made
-- them). The counts are not stored: they are derived from the replies after `last_read`.
CREATE TABLE thread_user_state (
    user_id         uuid    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    root_id         uuid    NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    conversation_id uuid    NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    following       boolean NOT NULL,
    -- They chose to stop following. Replies and mentions do not bring them back (replying does).
    unfollowed      boolean NOT NULL DEFAULT false,
    -- Position of the newest reply they have read; 0 means none.
    last_read       bigint  NOT NULL DEFAULT 0 CHECK (last_read >= 0),
    PRIMARY KEY (user_id, root_id),
    CHECK (NOT (following AND unfollowed))
);
