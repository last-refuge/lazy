-- Slash commands (llm-docs/DECISIONS.md, D36). Additive.

-- The secret an app's requests are signed with, as Slack signs its (a credential, D31; made when
-- first asked for).
ALTER TABLE apps ADD COLUMN signing_secret_id uuid REFERENCES integration_credentials (id);

-- A command an app answers: `/name` in a workspace goes to `url`. Names are unique per workspace.
CREATE TABLE app_commands (
    id           uuid PRIMARY KEY,
    app_id       uuid        NOT NULL REFERENCES apps (id) ON DELETE CASCADE,
    workspace_id uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name         text        NOT NULL CHECK (name ~ '^[a-z0-9][a-z0-9_-]{0,31}$'),
    -- https, or http to this machine (an app run beside the server, and tests).
    url          text        NOT NULL CHECK (url ~ '^https?://' AND length(url) <= 2048),
    description  text        NOT NULL DEFAULT '' CHECK (length(description) <= 200),
    usage_hint   text        NOT NULL DEFAULT '' CHECK (length(usage_hint) <= 100),
    created_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, name)
);

-- Where an app can send a later answer to one use of a command (Slack's `response_url`): only the
-- SHA-256 of the address's token is kept. Good for 30 minutes and 5 answers.
CREATE TABLE command_responses (
    token_hash      bytea PRIMARY KEY CHECK (octet_length(token_hash) = 32),
    app_id          uuid        NOT NULL REFERENCES apps (id) ON DELETE CASCADE,
    conversation_id uuid        NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    user_id         uuid        NOT NULL REFERENCES users (id),
    expires_at      timestamptz NOT NULL,
    uses            integer     NOT NULL DEFAULT 0
);
