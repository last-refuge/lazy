-- Lazy server schema, version 1. See llm-docs/SERVER.md for the design.

CREATE TABLE users (
    id            uuid PRIMARY KEY,
    -- Usernames are stored lowercase, so a plain unique constraint is case-insensitive.
    username      text        NOT NULL UNIQUE CHECK (username = lower(username)),
    display_name  text        NOT NULL,
    password_hash text        NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);

-- Only the SHA-256 of the bearer token is stored.
CREATE TABLE sessions (
    id         uuid PRIMARY KEY,
    user_id    uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    token_hash bytea       NOT NULL UNIQUE,
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    revoked    boolean     NOT NULL DEFAULT false
);
CREATE INDEX sessions_user_idx ON sessions (user_id);

CREATE TABLE workspaces (
    id         uuid PRIMARY KEY,
    name       text        NOT NULL,
    -- Position of the newest event in this workspace's log. Bumping it inside a transaction takes
    -- the row lock that makes event positions gapless and commit-ordered.
    event_head bigint      NOT NULL DEFAULT 0 CHECK (event_head >= 0),
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE workspace_members (
    workspace_id uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    user_id      uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    role         text        NOT NULL CHECK (role IN ('owner', 'admin', 'member')),
    joined_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, user_id)
);
CREATE INDEX workspace_members_user_idx ON workspace_members (user_id);

CREATE TABLE conversations (
    id           uuid PRIMARY KEY,
    workspace_id uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    kind         text        NOT NULL CHECK (kind IN ('channel')),
    name         text        NOT NULL CHECK (name = lower(name)),
    topic        text,
    visibility   text        NOT NULL CHECK (visibility IN ('public', 'private')),
    archived     boolean     NOT NULL DEFAULT false,
    created_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, name)
);

CREATE TABLE conversation_members (
    conversation_id uuid        NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    user_id         uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    joined_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (conversation_id, user_id)
);
CREATE INDEX conversation_members_user_idx ON conversation_members (user_id);

CREATE TABLE messages (
    id              uuid PRIMARY KEY,
    conversation_id uuid        NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    author_id       uuid        NOT NULL REFERENCES users (id),
    -- Versioned message document (lazy_domain::MessageDocument).
    document        jsonb       NOT NULL,
    -- Plain-text projection of `document`, reserved for full-text search.
    plain_text      text        NOT NULL,
    revision        integer     NOT NULL DEFAULT 1 CHECK (revision >= 1),
    -- Event-log position at creation; the ordering key for history.
    seq             bigint      NOT NULL CHECK (seq >= 1),
    created_at      timestamptz NOT NULL
);
CREATE UNIQUE INDEX messages_conversation_seq_idx ON messages (conversation_id, seq DESC);

-- Idempotency records. `message_id` is deferrable because the receipt is claimed before the
-- message row is inserted, within the same transaction.
CREATE TABLE operation_receipts (
    user_id      uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    operation_id uuid        NOT NULL,
    request_hash bytea       NOT NULL,
    message_id   uuid        NOT NULL REFERENCES messages (id) ON DELETE CASCADE
                             DEFERRABLE INITIALLY DEFERRED,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, operation_id)
);

-- The durable, per-workspace event log. Rows are never updated.
CREATE TABLE events (
    workspace_id    uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    seq             bigint      NOT NULL CHECK (seq >= 1),
    occurred_at     timestamptz NOT NULL,
    kind            text        NOT NULL,
    -- NULL for workspace-wide events, visible to every member.
    conversation_id uuid        REFERENCES conversations (id) ON DELETE CASCADE,
    operation_id    uuid,
    -- The serialized lazy_protocol::Event.
    payload         jsonb       NOT NULL,
    PRIMARY KEY (workspace_id, seq)
);
