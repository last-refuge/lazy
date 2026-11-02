-- Link previews (llm-docs/DECISIONS.md, D41). Additive.

-- What a linked page said about itself when it was last fetched, per workspace: a preview is only
-- ever read through a message the reader can see, and a workspace never learns which links
-- another fetched. `ok = false` remembers a failed fetch for a while, so it is not retried on
-- every view.
CREATE TABLE link_previews (
    workspace_id uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    url          text        NOT NULL CHECK (length(url) <= 2048),
    ok           boolean     NOT NULL,
    title        text        CHECK (length(title) <= 200),
    description  text        CHECK (length(description) <= 300),
    site_name    text        CHECK (length(site_name) <= 253),
    fetched_at   timestamptz NOT NULL,
    PRIMARY KEY (workspace_id, url)
);
CREATE INDEX link_previews_fetched_at ON link_previews (fetched_at);
