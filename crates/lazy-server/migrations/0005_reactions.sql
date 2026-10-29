-- Reactions to messages (protocol version 4). Additive: existing databases upgrade in place, and
-- every existing message simply has no reactions.

-- The log position of the latest change to a message's reactions (0 before any). It is the
-- version a client compares to decide which of two lists of reactions is newer.
ALTER TABLE messages
    ADD COLUMN reactions_as_of bigint NOT NULL DEFAULT 0 CHECK (reactions_as_of >= 0);

-- One row per (message, person, emoji) that is currently set. Setting a reaction inserts a row and
-- clearing it deletes the row, so the table is the current state and the event log is the history.
-- `emoji` is the canonical glyph (lazy_domain::ReactionEmoji), never a variant spelling, so one
-- reaction has one row however it was typed.
CREATE TABLE message_reactions (
    message_id uuid   NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    user_id    uuid   NOT NULL REFERENCES users (id),
    emoji      text   NOT NULL,
    -- The position of the change that added this reaction. It orders reactions: an emoji's place in
    -- the list is its smallest `seq`, and the people under it are in `seq` order.
    seq        bigint NOT NULL CHECK (seq >= 1),
    PRIMARY KEY (message_id, user_id, emoji)
);

-- A page of history reads the reactions of up to a hundred messages in the order they must be
-- listed, in one indexed scan.
CREATE INDEX message_reactions_by_message_idx ON message_reactions (message_id, seq);
