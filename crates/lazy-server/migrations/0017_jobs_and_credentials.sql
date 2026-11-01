-- Durable jobs and encrypted integration credentials (llm-docs/DECISIONS.md, D31). Additive.

-- Work done outside a request: delivering to or from another service, and anything else that must
-- survive a restart and be retried. A job is claimed by a worker with a lease that the worker
-- renews while it runs; a lease that runs out (the worker died) makes the job claimable again.
-- Jobs are at-least-once: a handler must be idempotent, or say the outcome is unknown.
CREATE TABLE jobs (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- Which handler runs it, e.g. 'github.event'.
    kind            text        NOT NULL CHECK (kind <> ''),
    -- The workspace it belongs to, if any: deleting the workspace deletes its jobs.
    workspace_id    uuid        REFERENCES workspaces (id) ON DELETE CASCADE,
    -- A second enqueue with the same (kind, dedupe_key) is the same job, e.g. a provider's
    -- delivery id. NULL never collides.
    dedupe_key      text,
    payload         jsonb       NOT NULL DEFAULT '{}'::jsonb,
    -- queued: waiting for run_after; running: leased; succeeded; failed: definitive, or out of
    -- attempts; unknown: it may or may not have happened, and nothing retries it by itself.
    state           text        NOT NULL DEFAULT 'queued'
                    CHECK (state IN ('queued', 'running', 'succeeded', 'failed', 'unknown')),
    attempts        integer     NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    max_attempts    integer     NOT NULL CHECK (max_attempts >= 1),
    run_after       timestamptz NOT NULL DEFAULT now(),
    lease_owner     uuid,
    lease_until     timestamptz,
    -- Why the last attempt did not succeed, in the handler's own words (never a payload or a
    -- credential), and of which kind.
    last_error_class text       CHECK (last_error_class IN ('retryable', 'definitive', 'unknown')),
    last_error      text,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    finished_at     timestamptz,
    CHECK ((state = 'running') = (lease_owner IS NOT NULL AND lease_until IS NOT NULL)),
    CHECK ((state IN ('succeeded', 'failed', 'unknown')) = (finished_at IS NOT NULL)),
    UNIQUE (kind, dedupe_key)
);

-- What a worker claims next.
CREATE INDEX jobs_queued_idx ON jobs (run_after, id) WHERE state = 'queued';
-- Leases that have run out.
CREATE INDEX jobs_running_idx ON jobs (lease_until) WHERE state = 'running';
-- A workspace's jobs, newest first, for its admins; and old finished jobs, for cleanup.
CREATE INDEX jobs_workspace_idx ON jobs (workspace_id, id DESC);
CREATE INDEX jobs_finished_idx ON jobs (finished_at) WHERE finished_at IS NOT NULL;

-- Secrets an integration holds for a workspace (an app's private key, an access token, a webhook
-- signing secret), encrypted with XChaCha20-Poly1305 under a server key that never enters the
-- database. The id, the workspace and the kind are bound into the encryption, so a row's
-- ciphertext cannot be moved to another row. Revoking clears the ciphertext and keeps the row.
CREATE TABLE integration_credentials (
    id           uuid PRIMARY KEY,
    workspace_id uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    kind         text        NOT NULL CHECK (kind <> ''),
    -- Which server key sealed it: the first 8 hex digits of the key's SHA-256.
    key_id       text        CHECK (key_id ~ '^[0-9a-f]{8}$'),
    nonce        bytea       CHECK (octet_length(nonce) = 24),
    ciphertext   bytea,
    created_at   timestamptz NOT NULL DEFAULT now(),
    rotated_at   timestamptz,
    revoked_at   timestamptz,
    CHECK ((revoked_at IS NULL) = (ciphertext IS NOT NULL)),
    CHECK ((ciphertext IS NULL) = (nonce IS NULL) AND (ciphertext IS NULL) = (key_id IS NULL))
);

CREATE INDEX integration_credentials_workspace_idx ON integration_credentials (workspace_id);
CREATE INDEX integration_credentials_key_idx ON integration_credentials (key_id)
    WHERE revoked_at IS NULL;
