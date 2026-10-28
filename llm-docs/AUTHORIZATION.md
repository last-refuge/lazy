# Authorization

Who may do what, as enforced by the server. The server is authoritative for every mutation, history read, search result, attachment request, realtime subscription, and huddle join; clients may use these rules only to decide what to offer. The pure rules live in `lazy-domain` (`ChannelAccess`) and are checked row by row in `crates/lazy-domain/tests/authz_matrix.rs`.

## Facts the rules use

| Fact | Source |
| --- | --- |
| Workspace role (`owner`, `admin`, `member`) or none | `workspace_members` |
| Channel membership | `conversation_members` |
| Visibility (`public`, `private`) and archived flag | `conversations` |

## Channel access

Two predicates decide everything. A workspace role never bypasses a private channel: owners and admins need channel membership like anyone else.

- **Read**: the person is in the workspace and the channel is public or they are a member.
- **Post**: the person is in the workspace, is a member of the channel, and the channel is not archived.

Posting implies reading. Archived channels stay readable.

| Role | Member | Visibility | Archived | Read | Post |
| --- | --- | --- | --- | --- | --- |
| none | any | any | any | no | no |
| member, admin, owner | no | public | no | yes | no |
| member, admin, owner | no | public | yes | yes | no |
| member, admin, owner | no | private | any | no | no |
| member, admin, owner | yes | public or private | no | yes | yes |
| member, admin, owner | yes | public or private | yes | yes | no |

All 32 combinations are listed individually in the test.

## Actions

| Action | Rule | On denial |
| --- | --- | --- |
| Register | Open registration is enabled (development default). The first registrant becomes the workspace owner | `username_taken`, `invalid_request` |
| Bootstrap | Returns only workspaces the caller belongs to and, in them, channels the caller may read | omitted |
| Send a message | Post | `not_found` if the caller cannot read the channel (so private channels do not reveal their existence), else `forbidden` |
| Read history | Read | `not_found` |
| Subscribe to a workspace | The caller is a workspace member | `forbidden` frame |
| Deliver a live or replayed event | Read for message events, rechecked against current membership; workspace-wide events (a member joined) go to every member | not delivered |
| Operation status | Only the operation's own author can look it up (receipts are keyed by user and operation id) | `not_found` |

## Planned actions

These are specified now so later phases do not invent rules.

| Action | Rule |
| --- | --- |
| Edit or delete a message | The author may; later, admins may delete. Requires Read (so a person removed from a private channel cannot edit their old messages) |
| Search | Results are filtered by Read at query time, so removal from a channel removes its messages from results immediately; snippets obey the same filter |
| Attachment download or upload | Read to download; Post to upload into a conversation |
| Thread reply | Inherits the conversation's Read and Post |
| Join a huddle in a conversation | Read, rechecked while connected |
| Manage channels and members | Admins and owners, plus channel creators for their private channels |

Membership removal takes effect on the next delivery, history read, or mutation: replay and live delivery use current membership, never the membership at subscribe time.
