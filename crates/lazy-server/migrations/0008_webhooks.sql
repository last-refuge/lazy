-- Incoming webhooks and bot identity (protocol version 7). Additive: existing databases upgrade in
-- place, and every existing user is a person.

-- ---------------------------------------------------------------------------------------------
-- Bots: identities that belong to an integration, not a person.
-- ---------------------------------------------------------------------------------------------

ALTER TABLE users
    ADD COLUMN is_bot boolean NOT NULL DEFAULT false,
    -- A bot has no password, only this marker, which is not a password hash and verifies nothing;
    -- a person always has an Argon2 hash. The two cannot be mixed up.
    ADD CONSTRAINT users_bot_has_no_password CHECK (is_bot = (password_hash = '!')),
    -- The target of the foreign key below.
    ADD CONSTRAINT users_id_is_bot_key UNIQUE (id, is_bot);

-- A session can only belong to a person: the pair (user, "is a bot") must exist in `users`, and
-- the column is constrained to false, so no session for a bot can ever be created (and a person
-- with a session cannot be turned into a bot).
ALTER TABLE sessions
    ADD COLUMN user_is_bot boolean NOT NULL DEFAULT false,
    ADD CONSTRAINT sessions_user_is_a_person CHECK (NOT user_is_bot),
    ADD CONSTRAINT sessions_user_kind_fkey
        FOREIGN KEY (user_id, user_is_bot) REFERENCES users (id, is_bot) ON DELETE CASCADE;

-- ---------------------------------------------------------------------------------------------
-- Webhooks
-- ---------------------------------------------------------------------------------------------

CREATE TABLE webhooks (
    id              uuid PRIMARY KEY,
    workspace_id    uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    conversation_id uuid        NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    -- The bot's display name.
    name            text        NOT NULL,
    -- The bot user that authors what is posted. Kept (and a member of nothing) after the webhook is
    -- revoked, so past messages keep their author.
    bot_user_id     uuid        NOT NULL UNIQUE REFERENCES users (id),
    -- SHA-256 of the secret in the address; NULL once revoked (so a revoked address cannot verify
    -- even by mistake). The secret itself is never stored.
    secret_hash     bytea,
    created_by      uuid        NOT NULL REFERENCES users (id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    revoked_at      timestamptz,
    CHECK ((revoked_at IS NULL) = (secret_hash IS NOT NULL)),
    CHECK (secret_hash IS NULL OR octet_length(secret_hash) = 32)
);

-- The webhooks of a conversation that are in use: the list, and the limit on how many there are.
CREATE INDEX webhooks_conversation_idx
    ON webhooks (conversation_id, created_at, id) WHERE revoked_at IS NULL;
