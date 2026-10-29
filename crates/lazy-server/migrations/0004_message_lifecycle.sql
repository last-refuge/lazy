-- Editing and deleting messages (protocol version 3). Additive: existing databases upgrade in
-- place, and every stored message keeps reading (version 1 documents are still valid documents).

-- ---------------------------------------------------------------------------------------------
-- Messages: when the latest edit committed, and whether the message was deleted. A deleted
-- message is a tombstone: the row keeps its id, author, and position, with the placeholder
-- document and an empty text projection.
-- ---------------------------------------------------------------------------------------------

ALTER TABLE messages
    ADD COLUMN edited_at timestamptz,
    ADD COLUMN deleted   boolean NOT NULL DEFAULT false;

-- The unread count scans a conversation's messages after a position and ignores the person's own
-- messages and deleted ones. Carrying both columns in the index keeps that an index-only scan.
DROP INDEX messages_conversation_seq_idx;
CREATE UNIQUE INDEX messages_conversation_seq_idx
    ON messages (conversation_id, seq DESC) INCLUDE (author_id, deleted);

-- ---------------------------------------------------------------------------------------------
-- Prior revisions of an edited message, kept on the server (never sent to clients) until the
-- message is deleted, which removes them. The current revision lives in `messages`.
-- ---------------------------------------------------------------------------------------------

CREATE TABLE message_revisions (
    message_id uuid        NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    revision   integer     NOT NULL CHECK (revision >= 1),
    author_id  uuid        NOT NULL REFERENCES users (id),
    document   jsonb       NOT NULL,
    -- When this revision was committed; NULL for the original, which was never an edit.
    edited_at  timestamptz,
    PRIMARY KEY (message_id, revision)
);

-- ---------------------------------------------------------------------------------------------
-- Events remember which message they are about, so that deleting a message can remove its
-- content from the events that carried it (the log would otherwise keep the text that deletion
-- is meant to erase, and replay it).
-- ---------------------------------------------------------------------------------------------

ALTER TABLE events ADD COLUMN message_id uuid;

UPDATE events SET message_id = (payload -> 'message' ->> 'id')::uuid
WHERE kind = 'message_created';

CREATE INDEX events_message_idx ON events (message_id) WHERE message_id IS NOT NULL;

-- ---------------------------------------------------------------------------------------------
-- Receipts also cover edits. An edit's receipt records the revision the edit produced (or, for an
-- edit that changed nothing, the revision it found), so a repeat of the same request can return
-- its original result even after the message has moved on.
-- ---------------------------------------------------------------------------------------------

ALTER TABLE operation_receipts ADD COLUMN message_revision integer;

ALTER TABLE operation_receipts DROP CONSTRAINT operation_receipts_result_shape;
ALTER TABLE operation_receipts
    ADD CONSTRAINT operation_receipts_result_shape CHECK (
        (kind IN ('send_message', 'edit_message')
            AND message_id IS NOT NULL AND conversation_id IS NULL)
        OR (kind IN ('create_channel', 'open_conversation')
            AND conversation_id IS NOT NULL AND message_id IS NULL)
    );
