-- Apps with bot tokens (llm-docs/DECISIONS.md, D35). Additive.

-- An app a workspace admin made: its bot, and the token a program uses as the bot. Only the
-- SHA-256 of the token is kept, and its last four characters to tell tokens apart.
CREATE TABLE apps (
    id           uuid PRIMARY KEY,
    workspace_id uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name         text        NOT NULL,
    bot_user_id  uuid        NOT NULL UNIQUE REFERENCES users (id),
    token_hash   bytea       UNIQUE CHECK (octet_length(token_hash) = 32),
    token_hint   text        NOT NULL DEFAULT '',
    scopes       text[]      NOT NULL DEFAULT '{}',
    created_by   uuid        NOT NULL REFERENCES users (id),
    created_at   timestamptz NOT NULL DEFAULT now(),
    last_used_at timestamptz,
    -- Deleted: the token no longer works and the bot has left its channels; the bot user stays so
    -- what it posted keeps its author.
    deleted_at   timestamptz
);

CREATE INDEX apps_workspace_idx ON apps (workspace_id) WHERE deleted_at IS NULL;
