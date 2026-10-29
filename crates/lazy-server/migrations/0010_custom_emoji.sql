-- Custom emoji (protocol version 9). Additive: existing databases upgrade in place, and every
-- workspace starts with none. The image is not here: it is a small PNG in the attachment store,
-- under the emoji's id. Reactions need no change: `message_reactions.emoji` is free text, and a
-- custom emoji is stored there as `custom:{uuid}` (lazy_domain::ReactionEmoji).

CREATE TABLE custom_emoji (
    -- Chosen by the uploader's client, so a retried upload is the same upload.
    id            uuid PRIMARY KEY,
    workspace_id  uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    -- Without the colons, always lowercase (lazy_domain::EmojiName).
    name          text        NOT NULL CHECK (name = lower(name)),
    created_by    uuid        NOT NULL REFERENCES users (id),
    created_at    timestamptz NOT NULL DEFAULT now(),
    -- Set when it is deleted. The row stays, so that messages and reactions that used it can still
    -- show `:name:`.
    deleted_at    timestamptz,
    -- What is stored: a PNG whose long side is at most 128 pixels.
    media_type    text        NOT NULL DEFAULT 'image/png',
    size          bigint      NOT NULL CHECK (size > 0),
    -- Of the bytes that were uploaded (not of the stored PNG), so that the same upload again can be
    -- recognised.
    sha256        bytea       NOT NULL CHECK (octet_length(sha256) = 32),
    width         integer     NOT NULL CHECK (width > 0),
    height        integer     NOT NULL CHECK (height > 0),
    -- The cleanup job has removed the image of a deleted emoji.
    image_removed boolean     NOT NULL DEFAULT false,
    CHECK (NOT image_removed OR deleted_at IS NOT NULL)
);

-- A name belongs to one live emoji of a workspace at a time; a deleted one frees it.
CREATE UNIQUE INDEX custom_emoji_live_name_idx
    ON custom_emoji (workspace_id, name) WHERE deleted_at IS NULL;

-- The workspace's list (live and deleted), in order.
CREATE INDEX custom_emoji_workspace_idx ON custom_emoji (workspace_id, name, id);

-- Deleted emoji whose images the cleanup job has not removed yet.
CREATE INDEX custom_emoji_cleanup_idx
    ON custom_emoji (deleted_at, id) WHERE deleted_at IS NOT NULL AND NOT image_removed;
