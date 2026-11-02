-- Rich messages and button presses (llm-docs/DECISIONS.md, D40). Additive.

-- An integration's blocks, drawn in place of the document by clients that know them. The document
-- stays the fallback text (search, notifications, older clients).
ALTER TABLE messages ADD COLUMN rich jsonb;
ALTER TABLE message_revisions ADD COLUMN rich jsonb;

-- Where an app is told that someone pressed one of its buttons (Slack's interactivity request
-- URL): https, or http to this machine.
ALTER TABLE apps
    ADD COLUMN interactivity_url text
        CHECK (interactivity_url ~ '^https?://' AND length(interactivity_url) <= 2048);

-- A `response_url` given for a button press can replace or delete the message the button is on.
ALTER TABLE command_responses
    ADD COLUMN message_id uuid REFERENCES messages (id) ON DELETE CASCADE;
