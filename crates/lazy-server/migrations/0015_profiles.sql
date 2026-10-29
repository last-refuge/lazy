-- Several workspaces, profiles and avatars (llm-docs/DECISIONS.md, D23). Additive.

-- Which picture a person has set as their avatar: 0 for none, otherwise a number that changes
-- whenever the picture does and is never reused (see `user_avatars.version`), so
-- `GET /v1/users/{id}/avatar?v=<this>` can be cached for good. Everything that builds a `User`
-- carries it.
ALTER TABLE users ADD COLUMN avatar_version integer NOT NULL DEFAULT 0 CHECK (avatar_version >= 0);

-- A person's avatar: a square PNG made from what they uploaded (the original is not kept).
-- `version` is the highest number the person's avatar has had, and stays when the picture is
-- removed (`png` is then NULL): the next picture gets the next number, so a cached `?v=` URL can
-- never show a picture other than the one it was cached for.
CREATE TABLE user_avatars (
    user_id    uuid        PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    version    integer     NOT NULL CHECK (version >= 1),
    png        bytea       CHECK (png IS NULL OR octet_length(png) > 0),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- The person who created a workspace with `PUT /v1/workspaces/{id}`; NULL for the workspace the
-- server seeded and for any made before this existed. Counts toward the limit on workspaces one
-- person may create. Registration without an invitation joins the oldest workspace that has no
-- creator (the seeded one), never one a person made.
ALTER TABLE workspaces ADD COLUMN created_by uuid REFERENCES users (id);
CREATE INDEX workspaces_created_by_idx ON workspaces (created_by) WHERE created_by IS NOT NULL;
