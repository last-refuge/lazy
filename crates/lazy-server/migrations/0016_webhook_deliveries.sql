-- Managing integrations inside Lazy (llm-docs/DECISIONS.md, D30). Additive.

-- What became of each request to a webhook's address whose secret was right. A log for admins to
-- troubleshoot with, not an event: it never orders anything people see. It holds no content (no
-- text, no idempotency key), and only the newest `MAX_WEBHOOK_DELIVERIES_KEPT` rows of a webhook
-- are kept (older ones are deleted as new ones arrive). A run of rate-limited requests is one row.
CREATE TABLE webhook_deliveries (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    webhook_id      uuid        NOT NULL REFERENCES webhooks (id) ON DELETE CASCADE,
    received_at     timestamptz NOT NULL DEFAULT now(),
    outcome         text        NOT NULL CHECK (outcome IN
                        ('posted', 'duplicate', 'rate_limited', 'too_large', 'invalid',
                         'archived', 'failed')),
    -- The message posted, or answered again for a duplicate. Not a foreign key: the log outlives
    -- nothing it describes, and a message row is never deleted anyway.
    message_id      uuid,
    body_bytes      integer     NOT NULL CHECK (body_bytes >= 0),
    idempotency_key boolean     NOT NULL,
    CHECK ((outcome IN ('posted', 'duplicate')) = (message_id IS NOT NULL))
);

-- A webhook's log, newest first; also its latest entry and trimming to the newest rows.
CREATE INDEX webhook_deliveries_webhook_idx ON webhook_deliveries (webhook_id, id DESC);

-- The webhooks of a workspace that are in use, for the workspace's list of integrations.
CREATE INDEX webhooks_workspace_idx
    ON webhooks (workspace_id, conversation_id, created_at, id) WHERE revoked_at IS NULL;
