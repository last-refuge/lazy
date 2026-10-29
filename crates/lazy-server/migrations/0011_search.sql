-- Search (llm-docs/DECISIONS.md, D19). Additive.
--
-- `messages.search_vector` is the full-text index of the plain-text projection, with the `simple`
-- configuration (no stemming and no language assumption, so every script is treated alike, and no
-- accent folding). It is a stored generated column, so an edit reindexes itself and a deletion
-- (whose text is emptied) leaves the index with the same statement. Adding it REWRITES the
-- `messages` table (every row is read and the vector computed), which holds an exclusive lock
-- for the duration; the cost is measured in llm-docs/SERVER.md.
ALTER TABLE messages
    ADD COLUMN search_vector tsvector
    GENERATED ALWAYS AS (to_tsvector('simple', plain_text)) STORED;
CREATE INDEX messages_search_idx ON messages USING gin (search_vector);

-- An attachment's file name is searchable too. Punctuation is turned into spaces first, so that
-- `quarterly_report-v2.pdf` is found by `report` (the text-search parser would keep it whole as one
-- file-name token). An explicit character list rather than `[:alnum:]`, so the result does not
-- depend on the database's locale. Only attached, not deleted files can be found, so only they are
-- indexed.
ALTER TABLE attachments
    ADD COLUMN name_vector tsvector
    GENERATED ALWAYS AS (
        to_tsvector('simple', regexp_replace(name, '[._/\\()\[\]{}+,;:#&@!=~-]+', ' ', 'g'))
    ) STORED;
CREATE INDEX attachments_name_search_idx ON attachments USING gin (name_vector)
    WHERE message_id IS NOT NULL AND deleted_at IS NULL;

-- Newest first across a workspace: filters-only searches, and text searches for a very common word
-- (the newest matches are found by walking back from the newest message). Positions are per
-- workspace; with several workspaces the query's own conditions pick out one.
CREATE INDEX messages_seq_idx ON messages (seq DESC) WHERE NOT deleted;
-- `from:`
CREATE INDEX messages_author_idx ON messages (author_id, seq DESC) WHERE NOT deleted;
-- `after:` and `before:` on their own
CREATE INDEX messages_created_idx ON messages (created_at) WHERE NOT deleted;
