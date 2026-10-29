-- Notification settings, conversation preferences, and status (llm-docs/DECISIONS.md, D20).
-- Additive. The server stores and syncs these; it does not decide who is notified (clients and a
-- future push worker evaluate lazy_domain::notification::decide), so there is no per-recipient row.

-- A person's settings for one workspace, as the validated `NotificationSettings` value. A row exists
-- only while the settings differ from the defaults. Any field added to that type later must have a
-- serde default, so that rows written before it still read.
CREATE TABLE notification_settings (
    user_id      uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    workspace_id uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    settings     jsonb       NOT NULL,
    updated_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, workspace_id)
);

-- A person's preference for one conversation. A row exists only while it says something: a level of
-- its own, or muted. Removed with the person's membership of the conversation.
CREATE TABLE conversation_notification_prefs (
    user_id         uuid    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    conversation_id uuid    NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    level           text    CHECK (level IN ('all', 'mentions', 'nothing')),
    muted           boolean NOT NULL DEFAULT false,
    PRIMARY KEY (user_id, conversation_id),
    CHECK (level IS NOT NULL OR muted)
);
-- The preferences of one workspace's conversations, for the bootstrap, are found through the
-- primary key (user first) and the conversation rows.

-- A person's status in one workspace. A row exists while a status is set; one past `expires_at` is
-- served as no status (compared with the database clock when read) and is replaced or removed by
-- the person's next change, never announced at expiry.
CREATE TABLE user_status (
    user_id      uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    workspace_id uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    text         text        NOT NULL,
    -- A `ReactionEmoji` in its canonical text form: a glyph, or `custom:` and an emoji id.
    emoji        text,
    expires_at   timestamptz,
    updated_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, workspace_id)
);

-- The thread inbox lists a person's followed threads newest activity first, paged by the position
-- of the latest reply. This lets it walk the threads from the newest and probe the person's state
-- for each (a page of 20 of 60,000 threads, a third of them followed, is about 60 probes, however
-- deep the cursor), instead of sorting every thread the person follows.
CREATE INDEX threads_activity_idx ON threads (last_reply_seq DESC);
