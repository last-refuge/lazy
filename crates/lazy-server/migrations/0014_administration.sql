-- Workspace administration (llm-docs/DECISIONS.md, D22): workspace settings, deactivated members,
-- and invitations. Additive.

-- The workspace's settings other than its name, as the validated `WorkspaceSettings` value. `{}` is
-- the defaults; any field added to that type later must keep a serde default so that older rows
-- still read.
ALTER TABLE workspaces ADD COLUMN settings jsonb NOT NULL DEFAULT '{}';

-- A deactivated membership gives no access (every check goes through `access::active_member!`, the
-- one predicate), keeps the person's messages, role and conversation memberships, and is restored
-- by clearing this column.
ALTER TABLE workspace_members ADD COLUMN deactivated_at timestamptz;

-- Invitations. Only a hash of the code is stored (the code is shown once, when the invitation is
-- made). `uses` is counted by one guarded UPDATE, so `max_uses` holds under races.
CREATE TABLE workspace_invitations (
    id           uuid        PRIMARY KEY,
    workspace_id uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    code_hash    bytea       NOT NULL UNIQUE CHECK (octet_length(code_hash) = 32),
    created_by   uuid        NOT NULL REFERENCES users (id),
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL,
    max_uses     integer     CHECK (max_uses IS NULL OR max_uses >= 1),
    uses         integer     NOT NULL DEFAULT 0 CHECK (uses >= 0),
    revoked_at   timestamptz
);
-- The listing: a workspace's invitations, newest first.
CREATE INDEX workspace_invitations_list_idx
    ON workspace_invitations (workspace_id, created_at DESC, id DESC);
