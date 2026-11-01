-- Webhook connectors (llm-docs/DECISIONS.md, D34): providers that post signed deliveries from a
-- webhook an admin makes in the provider's own settings (Linear, Jira). Additive.

-- A workspace's connection to one provider: its address (the id is in it), its bot, and its
-- signing secret, a credential (D31). At most one live connection per provider.
CREATE TABLE connectors (
    id              uuid PRIMARY KEY,
    workspace_id    uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    provider        text        NOT NULL CHECK (provider ~ '^[a-z]{1,32}$'),
    bot_user_id     uuid        NOT NULL UNIQUE REFERENCES users (id),
    secret_id       uuid        REFERENCES integration_credentials (id),
    created_by      uuid        NOT NULL REFERENCES users (id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    -- Disconnected: the secret is revoked and deliveries are refused; connecting again makes a
    -- new row (and a new address).
    disconnected_at timestamptz
);

CREATE UNIQUE INDEX connectors_live_idx ON connectors (workspace_id, provider)
    WHERE disconnected_at IS NULL;

-- Which team's or project's activity goes to which channel, and which kinds of it. An empty
-- scope follows everything the provider sends.
CREATE TABLE connector_subscriptions (
    id              uuid PRIMARY KEY,
    connector_id    uuid        NOT NULL REFERENCES connectors (id) ON DELETE CASCADE,
    workspace_id    uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    conversation_id uuid        NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    scope           text        NOT NULL DEFAULT ''
                                CHECK (scope = upper(scope) AND scope ~ '^[A-Z0-9_]{0,32}$'),
    events          text[]      NOT NULL CHECK (cardinality(events) >= 1),
    created_by      uuid        NOT NULL REFERENCES users (id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (connector_id, conversation_id, scope)
);

CREATE INDEX connector_subscriptions_route_idx ON connector_subscriptions (connector_id, scope);
