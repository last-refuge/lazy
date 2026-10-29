-- Mentions and user groups (protocol version 5). Additive: existing databases upgrade in place, and
-- every existing message simply mentions nobody.

-- ---------------------------------------------------------------------------------------------
-- Who a message notifies, decided when it is sent or edited and stored with it.
-- ---------------------------------------------------------------------------------------------

-- The exact `Mentions` value returned on the message (`{}` when it mentions nobody), so history and
-- events report what was decided at the time without recomputing it.
ALTER TABLE messages
    ADD COLUMN mentions         jsonb   NOT NULL DEFAULT '{}'::jsonb,
    -- `@channel` was used. Denormalized from `mentions` so the count below can use an index.
    ADD COLUMN mentions_channel boolean NOT NULL DEFAULT false;

-- One row per person a message notifies by name (directly or through a group). Counting a person's
-- unread mentions in a conversation is a range scan of this table; it is not derived from `messages`.
-- `seq` is the message's own position (not an edit's), because unread is measured by message
-- position. Rows are removed when the message is edited to stop mentioning someone, and when it is
-- deleted.
CREATE TABLE message_mentions (
    message_id      uuid   NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    user_id         uuid   NOT NULL REFERENCES users (id),
    conversation_id uuid   NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    seq             bigint NOT NULL CHECK (seq >= 1),
    PRIMARY KEY (message_id, user_id)
);

CREATE INDEX message_mentions_user_idx
    ON message_mentions (user_id, conversation_id, seq) INCLUDE (message_id);

-- Messages that used `@channel`, for the other half of the count. Partial, so it only holds those.
-- It carries `id` (to count a message once when it also names the person) and `author_id` (the
-- author is not notified), so the count never visits the table.
CREATE INDEX messages_channel_mentions_idx
    ON messages (conversation_id, seq) INCLUDE (author_id, id)
    WHERE mentions_channel AND NOT deleted;

-- A prior revision remembers who it notified, so an edit repeated after the message moved on can
-- return its original result: the old words together with the people they mentioned.
ALTER TABLE message_revisions
    ADD COLUMN mentions jsonb NOT NULL DEFAULT '{}'::jsonb;

-- ---------------------------------------------------------------------------------------------
-- User groups: a named set of people from one workspace that a message can mention together.
-- ---------------------------------------------------------------------------------------------

CREATE TABLE user_groups (
    id           uuid PRIMARY KEY,
    workspace_id uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    -- What it is mentioned by: a lowercase mention handle (lazy_domain::GroupHandle).
    handle       text        NOT NULL CHECK (handle = lower(handle)),
    name         text        NOT NULL,
    created_by   uuid        REFERENCES users (id),
    created_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, handle)
);

CREATE TABLE user_group_members (
    group_id uuid NOT NULL REFERENCES user_groups (id) ON DELETE CASCADE,
    user_id  uuid NOT NULL REFERENCES users (id),
    PRIMARY KEY (group_id, user_id)
);

-- ---------------------------------------------------------------------------------------------
-- Receipts also cover creating a group. The group may be deleted later, so `group_id` is not a
-- foreign key: the receipt must outlive it, or a repeated create would make the group again.
-- ---------------------------------------------------------------------------------------------

ALTER TABLE operation_receipts ADD COLUMN group_id uuid;

ALTER TABLE operation_receipts DROP CONSTRAINT operation_receipts_result_shape;
ALTER TABLE operation_receipts
    ADD CONSTRAINT operation_receipts_result_shape CHECK (
        (kind IN ('send_message', 'edit_message')
            AND message_id IS NOT NULL AND conversation_id IS NULL AND group_id IS NULL)
        OR (kind IN ('create_channel', 'open_conversation')
            AND conversation_id IS NOT NULL AND message_id IS NULL AND group_id IS NULL)
        OR (kind = 'create_group'
            AND group_id IS NOT NULL AND message_id IS NULL AND conversation_id IS NULL)
    );
