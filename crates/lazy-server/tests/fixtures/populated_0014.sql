-- What the administration release (migration 0014, protocol 13) added to the earlier fixtures: the
-- workspace's settings (channel creation for admins only), a deactivated person, and two invitations
-- to the workspace, one with uses left and one revoked. Apply after migration 0014.
--
--   dora ...00d4 (member, deactivated); invitations ...0b01 (3 uses, 1 used; code
--   `upgrade-code-0014-abcdef`) and ...0b02 (revoked; code `upgrade-code-0014-revoked`)

UPDATE workspaces SET settings = '{"channel_creation":"admins_only"}'
 WHERE id = '01a00000-0000-7000-8000-0000000000a0';

INSERT INTO users (id, username, display_name, password_hash) VALUES
    ('01a00000-0000-7000-8000-0000000000d4', 'dora', 'Dora',
     '$argon2id$v=19$m=8,t=1,p=1$c2FsdA$aGFzaA');
INSERT INTO workspace_members (workspace_id, user_id, role, deactivated_at) VALUES
    ('01a00000-0000-7000-8000-0000000000a0', '01a00000-0000-7000-8000-0000000000d4', 'member',
     '2026-10-02T09:00:00Z');

INSERT INTO workspace_invitations (id, workspace_id, code_hash, created_by, created_at, expires_at,
                                   max_uses, uses, revoked_at) VALUES
    ('01a00000-0000-7000-8000-000000000b01', '01a00000-0000-7000-8000-0000000000a0',
     sha256(convert_to('upgrade-code-0014-abcdef', 'UTF8')), '01a00000-0000-7000-8000-000000000001',
     now() - interval '1 day', now() + interval '6 days', 3, 1, NULL),
    ('01a00000-0000-7000-8000-000000000b02', '01a00000-0000-7000-8000-0000000000a0',
     sha256(convert_to('upgrade-code-0014-revoked', 'UTF8')), '01a00000-0000-7000-8000-000000000001',
     now() - interval '1 day', now() + interval '6 days', NULL, 0, now() - interval '1 hour');
