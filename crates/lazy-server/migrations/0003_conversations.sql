-- Conversations of every kind, read state, and stored event audiences (protocol version 2).

-- ---------------------------------------------------------------------------------------------
-- Conversations: channels, direct conversations, and groups.
-- ---------------------------------------------------------------------------------------------

ALTER TABLE conversations DROP CONSTRAINT conversations_kind_check;

ALTER TABLE conversations
    ADD CONSTRAINT conversations_kind_check CHECK (kind IN ('channel', 'direct', 'group')),
    -- Direct and group conversations have no name.
    ALTER COLUMN name DROP NOT NULL,
    ADD COLUMN description     text,
    ADD COLUMN created_by      uuid REFERENCES users (id),
    -- The workspace's default channel: everyone belongs, and it cannot be left or archived.
    ADD COLUMN is_default      boolean NOT NULL DEFAULT false,
    -- For direct and group conversations: the sorted participant ids joined with ',' (the
    -- domain's ParticipantSet::key). Unique per workspace, which makes opening a conversation
    -- idempotent even under concurrent requests.
    ADD COLUMN participant_key text,
    -- Maintained by every transaction that adds or removes a member.
    ADD COLUMN member_count    integer NOT NULL DEFAULT 0 CHECK (member_count >= 0);

ALTER TABLE conversations
    ADD CONSTRAINT conversations_name_iff_channel CHECK ((kind = 'channel') = (name IS NOT NULL)),
    ADD CONSTRAINT conversations_people_are_private CHECK (kind = 'channel' OR visibility = 'private'),
    ADD CONSTRAINT conversations_participant_key_iff_people CHECK ((kind = 'channel') = (participant_key IS NULL));

CREATE UNIQUE INDEX conversations_participant_key_idx
    ON conversations (workspace_id, participant_key) WHERE participant_key IS NOT NULL;
CREATE UNIQUE INDEX conversations_default_idx
    ON conversations (workspace_id) WHERE is_default;

-- Databases created before this migration: #general was the default channel.
UPDATE conversations SET is_default = true WHERE kind = 'channel' AND name = 'general' AND visibility = 'public';
UPDATE conversations c
SET member_count = (SELECT count(*) FROM conversation_members cm WHERE cm.conversation_id = c.id);

-- ---------------------------------------------------------------------------------------------
-- The unread count scans a conversation's messages after a position and filters on the author.
-- Carrying the author in the index lets that run as an index-only scan, proportional to the
-- unread messages and independent of the conversation's history.
-- ---------------------------------------------------------------------------------------------

DROP INDEX messages_conversation_seq_idx;
CREATE UNIQUE INDEX messages_conversation_seq_idx
    ON messages (conversation_id, seq DESC) INCLUDE (author_id);

-- ---------------------------------------------------------------------------------------------
-- Per-person conversation state: favorites and read positions. A row exists exactly while the
-- person is a member of the conversation.
-- ---------------------------------------------------------------------------------------------

CREATE TABLE conversation_user_state (
    user_id         uuid    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    conversation_id uuid    NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    favorite        boolean NOT NULL DEFAULT false,
    -- Position of the newest message the person has read; 0 means none.
    last_read       bigint  NOT NULL DEFAULT 0 CHECK (last_read >= 0),
    PRIMARY KEY (user_id, conversation_id)
);

-- Existing members start with everything read.
INSERT INTO conversation_user_state (user_id, conversation_id, last_read)
SELECT cm.user_id, cm.conversation_id,
       COALESCE((SELECT max(m.seq) FROM messages m WHERE m.conversation_id = cm.conversation_id), 0)
FROM conversation_members cm;

-- ---------------------------------------------------------------------------------------------
-- Event audiences. Stored with each event at append time and used for both replay and live
-- delivery, always evaluated against *current* membership:
--
--   workspace         every member of the workspace
--   members           the members of `conversation_id`
--   members_and_user  the members of `conversation_id`, and `target_user_id` even if they are not
--                     (or no longer) a member; used for removals
--   user              `target_user_id` only, and only while they are a member of `conversation_id`
--                     (when it is set)
-- ---------------------------------------------------------------------------------------------

ALTER TABLE events
    ADD COLUMN audience       text,
    ADD COLUMN target_user_id uuid;

UPDATE events SET audience = CASE kind WHEN 'member_joined' THEN 'workspace' ELSE 'members' END;

ALTER TABLE events
    ALTER COLUMN audience SET NOT NULL,
    ADD CONSTRAINT events_audience_shape CHECK (
        (audience = 'workspace' AND conversation_id IS NULL AND target_user_id IS NULL)
        OR (audience = 'members' AND conversation_id IS NOT NULL)
        OR (audience = 'members_and_user' AND conversation_id IS NOT NULL AND target_user_id IS NOT NULL)
        OR (audience = 'user' AND target_user_id IS NOT NULL)
    );

-- ---------------------------------------------------------------------------------------------
-- Operation receipts now cover creating channels and opening conversations as well as sends.
-- ---------------------------------------------------------------------------------------------

ALTER TABLE operation_receipts
    ADD COLUMN kind            text NOT NULL DEFAULT 'send_message',
    ALTER COLUMN message_id DROP NOT NULL,
    ADD COLUMN conversation_id uuid REFERENCES conversations (id) ON DELETE CASCADE
                                    DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE operation_receipts
    ADD CONSTRAINT operation_receipts_result_shape CHECK (
        (kind = 'send_message' AND message_id IS NOT NULL AND conversation_id IS NULL)
        OR (kind IN ('create_channel', 'open_conversation')
            AND conversation_id IS NOT NULL AND message_id IS NULL)
    );
