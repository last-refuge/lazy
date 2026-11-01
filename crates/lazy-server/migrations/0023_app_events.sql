-- Events for apps (llm-docs/DECISIONS.md, D38): where an app is told what happens in its channels,
-- as Slack's Events API does. Additive.
ALTER TABLE apps
    ADD COLUMN event_url text CHECK (event_url ~ '^https?://' AND length(event_url) <= 2048),
    ADD COLUMN event_types text[] NOT NULL DEFAULT '{}';
