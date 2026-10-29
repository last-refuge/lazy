-- Attachments (protocol version 8). Additive: existing databases upgrade in place, and every
-- existing message simply has no files. The bytes are not here: they live in the attachment store
-- (a directory on the server's disk), and this table holds what the server must decide about them.

CREATE TABLE attachments (
    -- Chosen by the client, so a retried upload is the same upload.
    id              uuid PRIMARY KEY,
    workspace_id    uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    conversation_id uuid        NOT NULL REFERENCES conversations (id) ON DELETE CASCADE,
    uploader_id     uuid        NOT NULL REFERENCES users (id),
    -- Already made safe (lazy_domain::attachment::sanitize_file_name).
    name            text        NOT NULL,
    -- What the server decided from the first bytes, not what the uploader said.
    media_type      text        NOT NULL,
    size            bigint      NOT NULL CHECK (size >= 0),
    sha256          bytea       NOT NULL CHECK (octet_length(sha256) = 32),
    -- Of images the server decoded.
    width           integer     CHECK (width > 0),
    height          integer     CHECK (height > 0),
    has_thumbnail   boolean     NOT NULL DEFAULT false,
    thumbnail_type  text,
    -- The message this file is on; NULL while it is pending (visible to its uploader only).
    message_id      uuid        REFERENCES messages (id) ON DELETE CASCADE,
    -- Its place among the message's files.
    ordinal         smallint    CHECK (ordinal >= 0),
    created_at      timestamptz NOT NULL DEFAULT now(),
    -- Set when the file can no longer be read: its message was deleted, or it was never sent and
    -- its time ran out. The cleanup job removes the bytes, then the row.
    deleted_at      timestamptz,
    CHECK ((width IS NULL) = (height IS NULL)),
    CHECK (has_thumbnail = (thumbnail_type IS NOT NULL)),
    CHECK (NOT has_thumbnail OR width IS NOT NULL),
    CHECK ((message_id IS NULL) = (ordinal IS NULL))
);

-- The files of a page of messages, in order, with one lookup.
CREATE INDEX attachments_message_idx ON attachments (message_id, ordinal) WHERE message_id IS NOT NULL;

-- What one person has waiting to be sent: the limit on how much, and which have run out of time.
CREATE INDEX attachments_pending_idx
    ON attachments (uploader_id, created_at) WHERE message_id IS NULL AND deleted_at IS NULL;
CREATE INDEX attachments_pending_age_idx
    ON attachments (created_at) WHERE message_id IS NULL AND deleted_at IS NULL;

-- Files waiting for the cleanup job to remove their bytes.
CREATE INDEX attachments_deleted_idx ON attachments (deleted_at, id) WHERE deleted_at IS NOT NULL;
