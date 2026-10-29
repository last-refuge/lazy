-- Discovery (llm-docs/DECISIONS.md, D21): saved messages, and indexes for the directories. Additive.

-- A person's saved messages. `saved_order` is assigned by the server when the message is saved
-- (larger is later; saving what is saved keeps it, removing and saving again gets a new one). It
-- orders and pages the list and means nothing else (it is not an event position). The message
-- stays here when it is deleted or its conversation becomes unreadable; the list and the bootstrap
-- leave such items out, and they return if the message can be read again.
CREATE TABLE saved_messages (
    user_id     uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    message_id  uuid        NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    saved_order bigint      GENERATED ALWAYS AS IDENTITY,
    saved_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, message_id)
);
-- The list, newest saved first, and its cursor.
CREATE INDEX saved_messages_list_idx ON saved_messages (user_id, saved_order DESC);
-- Deleting a message row (not done by the server, which keeps tombstones) or a conversation
-- cascades here by message.
CREATE INDEX saved_messages_message_idx ON saved_messages (message_id);

-- The channel directory lists a workspace's public channels by name or by most members first, with
-- the id as the final tie-break so that paging is stable; these let the first page of either be read
-- in order without sorting the workspace's channels. A text query still reads each channel (a
-- substring cannot use an index), which is bounded by the number of public channels.
CREATE INDEX conversations_directory_name_idx
    ON conversations (workspace_id, name, id) WHERE kind = 'channel' AND visibility = 'public';
CREATE INDEX conversations_directory_members_idx
    ON conversations (workspace_id, member_count DESC, name, id)
    WHERE kind = 'channel' AND visibility = 'public';

-- The people directory orders by display name (case-insensitive), then username, then id.
CREATE INDEX users_directory_idx ON users (lower(display_name), username, id);
