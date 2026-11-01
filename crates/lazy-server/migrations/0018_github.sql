-- The GitHub connector (llm-docs/DECISIONS.md, D32). Additive.

-- A started "Connect GitHub": the one-time state that ties GitHub's redirect back to the admin
-- and workspace that asked. Only the SHA-256 of the state is kept.
CREATE TABLE github_setups (
    state_hash   bytea PRIMARY KEY CHECK (octet_length(state_hash) = 32),
    workspace_id uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    user_id      uuid        NOT NULL REFERENCES users (id),
    -- The GitHub organization the app is created in.
    organization text        NOT NULL CHECK (organization ~ '^[A-Za-z0-9](?:[A-Za-z0-9-]{0,38})$'),
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,
    used_at      timestamptz
);

-- The GitHub App a workspace created for itself (one per workspace). Its secrets are credentials
-- (D31); this row holds only references to them.
CREATE TABLE github_apps (
    workspace_id      uuid PRIMARY KEY REFERENCES workspaces (id) ON DELETE CASCADE,
    app_id            bigint      NOT NULL UNIQUE,
    slug              text        NOT NULL,
    owner_login       text        NOT NULL,
    html_url          text        NOT NULL,
    -- The bot that posts what GitHub sends, shown as the app's name with an "APP" badge.
    bot_user_id       uuid        NOT NULL UNIQUE REFERENCES users (id),
    private_key_id    uuid        REFERENCES integration_credentials (id),
    webhook_secret_id uuid        REFERENCES integration_credentials (id),
    client_secret_id  uuid        REFERENCES integration_credentials (id),
    created_by        uuid        NOT NULL REFERENCES users (id),
    created_at        timestamptz NOT NULL DEFAULT now(),
    -- Disconnected: the credentials are revoked, nothing is accepted from GitHub, and a new
    -- connection replaces this row.
    disconnected_at   timestamptz
);

-- Where the app is installed, as GitHub's signed `installation` events say.
CREATE TABLE github_installations (
    installation_id      bigint PRIMARY KEY,
    workspace_id         uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    -- The app it is an installation of (a reconnected workspace has a new app).
    app_id               bigint      NOT NULL,
    account_login        text        NOT NULL,
    -- 'all' or 'selected'.
    repository_selection text        NOT NULL,
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now(),
    suspended_at         timestamptz,
    removed_at           timestamptz
);

CREATE INDEX github_installations_workspace_idx ON github_installations (workspace_id);

-- Which repository's activity goes to which channel, and which kinds of it.
CREATE TABLE github_subscriptions (
    id              uuid PRIMARY KEY,
    workspace_id    uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    conversation_id uuid        NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    -- `owner/name`, lower case.
    repository      text        NOT NULL CHECK (repository = lower(repository)
                                                AND repository ~ '^[a-z0-9-]+/[a-z0-9._-]+$'),
    events          text[]      NOT NULL CHECK (cardinality(events) >= 1),
    created_by      uuid        NOT NULL REFERENCES users (id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (conversation_id, repository)
);

CREATE INDEX github_subscriptions_repo_idx ON github_subscriptions (workspace_id, repository);
