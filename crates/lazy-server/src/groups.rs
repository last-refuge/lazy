//! User groups: named sets of people that a message can mention together (`@design-team`).
//!
//! Only admins and owners create, change, and delete groups; anyone in the workspace can list them.
//! A workspace holds at most [`MAX_GROUPS_PER_WORKSPACE`] groups of at most [`MAX_GROUP_MEMBERS`]
//! people, all from the workspace.
//!
//! # One namespace for handles
//!
//! A group's handle is what it is mentioned by, so it must not collide with a person's username,
//! another group's handle in the workspace, or a reserved word. The two ways to claim a handle are
//! creating or renaming a group and registering a person, and they race: each checks that the
//! handle is free and then takes it. Both therefore take a per-handle advisory lock
//! ([`lock_handle`]) *before* checking and hold it to commit, so whichever comes second sees the
//! first's row and is refused. Locks are taken in the order *per-workspace group lock, handle lock,
//! group row, workspace position*, and registration takes the handle lock before anything but its
//! own turn-taking lock, so none of this can deadlock with the rest of the server.
//!
//! Messages that mentioned a group keep naming it by identity and keep the people it notified, so
//! changing or deleting a group never rewrites a message.

use lazy_domain::mention::handle_is_reserved;
use lazy_domain::{
    DisplayName, GroupHandle, GroupId, MAX_GROUP_MEMBERS, MAX_GROUPS_PER_WORKSPACE, OperationId,
    UserId, WorkspaceId,
};
use lazy_protocol::{
    CreateGroupRequest, ErrorCode, Event, GroupsResponse, UpdateGroupRequest, UserGroup,
};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::accounts;
use crate::crypto;
use crate::error::AppError;
use crate::events::{self, Audience, PendingEvent};
use crate::receipts::{self, ReceiptKind, ReceiptTarget};

/// Takes the advisory lock for one handle, held until the transaction ends. Everything that claims
/// a handle (a group, a person) takes it before checking that the handle is free.
pub(crate) async fn lock_handle(conn: &mut PgConnection, handle: &str) -> Result<(), AppError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("handle:{handle}"))
        .execute(conn)
        .await?;
    Ok(())
}

/// Takes the lock that makes creating groups in one workspace take turns, so the limit on how many
/// there are is exact.
async fn lock_group_creation(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<(), AppError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("groups:{workspace}"))
        .execute(conn)
        .await?;
    Ok(())
}

/// Whether any workspace has a group with this handle. Registration refuses such a username:
/// a handle that names a group must not also name a person.
pub(crate) async fn handle_names_a_group(
    conn: &mut PgConnection,
    handle: &str,
) -> Result<bool, AppError> {
    let taken: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM user_groups WHERE handle = $1)")
            .bind(handle)
            .fetch_one(conn)
            .await?;
    Ok(taken)
}

async fn handle_names_a_person(conn: &mut PgConnection, handle: &str) -> Result<bool, AppError> {
    let taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE username = $1)")
        .bind(handle)
        .fetch_one(conn)
        .await?;
    Ok(taken)
}

fn name_taken() -> AppError {
    AppError::new(
        ErrorCode::NameTaken,
        "that handle is already used by a person or a group",
    )
}

fn group_not_found() -> AppError {
    AppError::not_found("group not found")
}

/// A workspace member who may manage groups, or the error to return: `not_found` for someone
/// outside the workspace, `forbidden` for a member who is not an admin or owner.
async fn require_manager(
    pool: &PgPool,
    workspace: WorkspaceId,
    actor: UserId,
) -> Result<(), AppError> {
    match accounts::workspace_role(pool, workspace, actor).await? {
        None => Err(AppError::not_found("workspace not found")),
        Some(role) if role.can_manage_groups() => Ok(()),
        Some(_) => Err(AppError::forbidden(
            "only admins and owners manage user groups",
        )),
    }
}

/// Checks a membership list: no more than the limit once duplicates are gone, and everyone in the
/// workspace. Returns the people sorted by id, each once.
async fn checked_members(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    members: &[UserId],
) -> Result<Vec<UserId>, AppError> {
    let mut members = members.to_vec();
    members.sort();
    members.dedup();
    if members.len() > MAX_GROUP_MEMBERS {
        return Err(AppError::new(
            ErrorCode::LimitReached,
            format!("a group can have at most {MAX_GROUP_MEMBERS} people"),
        ));
    }
    if !members.is_empty() {
        let ids: Vec<Uuid> = members.iter().map(|user| user.as_uuid()).collect();
        let found: i64 = sqlx::query_scalar(concat!(
            "SELECT count(*) FROM workspace_members wm JOIN users u ON u.id = wm.user_id \
             WHERE wm.workspace_id = $1 AND wm.user_id = ANY($2) AND NOT u.is_bot AND ",
            crate::access::active_member!("wm")
        ))
        .bind(workspace.as_uuid())
        .bind(&ids)
        .fetch_one(conn)
        .await?;
        if usize::try_from(found).ok() != Some(members.len()) {
            return Err(AppError::invalid(
                "everyone in a group must be a person in the workspace",
            ));
        }
    }
    Ok(members)
}

#[derive(sqlx::FromRow)]
struct GroupRow {
    id: Uuid,
    workspace_id: Uuid,
    handle: String,
    name: String,
}

impl GroupRow {
    fn into_group(self, members: Vec<UserId>) -> Result<UserGroup, AppError> {
        Ok(UserGroup {
            id: GroupId::from_uuid(self.id),
            workspace_id: WorkspaceId::from_uuid(self.workspace_id),
            handle: GroupHandle::parse(&self.handle)
                .map_err(|_| AppError::internal("stored group handle is invalid"))?,
            name: DisplayName::parse(&self.name)
                .map_err(|_| AppError::internal("stored group name is invalid"))?,
            members,
        })
    }
}

/// One group with its members, or `None` if it does not exist.
pub async fn load_group(
    conn: &mut PgConnection,
    id: GroupId,
) -> Result<Option<UserGroup>, AppError> {
    let row: Option<GroupRow> =
        sqlx::query_as("SELECT id, workspace_id, handle, name FROM user_groups WHERE id = $1")
            .bind(id.as_uuid())
            .fetch_optional(&mut *conn)
            .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let members: Vec<Uuid> = sqlx::query_scalar(
        "SELECT user_id FROM user_group_members WHERE group_id = $1 ORDER BY user_id",
    )
    .bind(id.as_uuid())
    .fetch_all(conn)
    .await?;
    Ok(Some(row.into_group(
        members.into_iter().map(UserId::from_uuid).collect(),
    )?))
}

/// Every group of a workspace, ordered by handle, with members in a fixed order. Two queries for
/// all of them.
pub async fn groups_in_workspace(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<Vec<UserGroup>, AppError> {
    let rows: Vec<GroupRow> = sqlx::query_as(
        "SELECT id, workspace_id, handle, name FROM user_groups WHERE workspace_id = $1 ORDER BY handle",
    )
    .bind(workspace.as_uuid())
    .fetch_all(&mut *conn)
    .await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let pairs: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT group_id, user_id FROM user_group_members WHERE group_id = ANY($1) ORDER BY user_id",
    )
    .bind(&ids)
    .fetch_all(conn)
    .await?;
    let mut members: std::collections::HashMap<Uuid, Vec<UserId>> = Default::default();
    for (group, user) in pairs {
        members
            .entry(group)
            .or_default()
            .push(UserId::from_uuid(user));
    }
    rows.into_iter()
        .map(|row| {
            let people = members.remove(&row.id).unwrap_or_default();
            row.into_group(people)
        })
        .collect()
}

/// The groups of a workspace, for anyone in it.
pub async fn list_groups(
    pool: &PgPool,
    actor: UserId,
    workspace: WorkspaceId,
) -> Result<GroupsResponse, AppError> {
    accounts::workspace_role(pool, workspace, actor)
        .await?
        .ok_or_else(|| AppError::not_found("workspace not found"))?;
    let mut conn = pool.acquire().await?;
    Ok(GroupsResponse {
        groups: groups_in_workspace(&mut conn, workspace).await?,
    })
}

/// The group an earlier `create_group` operation made, if it still exists.
pub async fn operation_group(
    conn: &mut PgConnection,
    user: UserId,
    operation: OperationId,
) -> Result<Option<UserGroup>, AppError> {
    let Some(receipt) = receipts::find(conn, user, operation).await? else {
        return Ok(None);
    };
    match receipt.group_id {
        Some(id) => load_group(conn, GroupId::from_uuid(id)).await,
        None => Ok(None),
    }
}

/// Creates a user group. Admins and owners only.
///
/// An operation like a channel creation: the same id with the same request returns the same group
/// (or `conflict` if it has since been deleted, since the receipt must not let a repeat make a
/// second one); the same id with anything different is a `conflict`. A handle that names a person
/// or another group in the workspace is `name_taken`; more than 100 groups, or more than 200
/// people, is `limit_reached`; someone outside the workspace is `invalid_request`.
pub async fn create_group(
    pool: &PgPool,
    actor: UserId,
    workspace: WorkspaceId,
    request: CreateGroupRequest,
) -> Result<UserGroup, AppError> {
    require_manager(pool, workspace, actor).await?;
    let mut conn = pool.acquire().await?;
    let members = checked_members(&mut conn, workspace, &request.members).await?;
    drop(conn);

    let member_bytes: Vec<u8> = members
        .iter()
        .flat_map(|user| *user.as_uuid().as_bytes())
        .collect();
    let fingerprint = crypto::command_fingerprint(
        "create_group",
        &[
            workspace.as_uuid().as_bytes(),
            request.handle.as_str().as_bytes(),
            request.name.as_str().as_bytes(),
            &member_bytes,
        ],
    );
    let id = GroupId::generate();
    let mut tx = pool.begin().await?;
    if let Some(existing) = receipts::claim(
        &mut tx,
        actor,
        request.operation_id,
        ReceiptKind::CreateGroup,
        &fingerprint,
        ReceiptTarget::Group(id),
    )
    .await?
    {
        existing.ensure_same(ReceiptKind::CreateGroup, &fingerprint)?;
        let original = existing
            .group_id
            .map(GroupId::from_uuid)
            .ok_or_else(|| AppError::internal("receipt has no group"))?;
        return load_group(&mut tx, original).await?.ok_or_else(|| {
            AppError::conflict("this operation created a group that has since been deleted")
        });
    }

    // Take turns with other creations (for the limit) and with anyone claiming this handle, then
    // check; see the module docs for why the order matters.
    lock_group_creation(&mut tx, workspace).await?;
    lock_handle(&mut tx, request.handle.as_str()).await?;
    let existing: i64 =
        sqlx::query_scalar("SELECT count(*) FROM user_groups WHERE workspace_id = $1")
            .bind(workspace.as_uuid())
            .fetch_one(&mut *tx)
            .await?;
    if usize::try_from(existing).unwrap_or(usize::MAX) >= MAX_GROUPS_PER_WORKSPACE {
        return Err(AppError::new(
            ErrorCode::LimitReached,
            format!("a workspace can have at most {MAX_GROUPS_PER_WORKSPACE} groups"),
        ));
    }
    if handle_names_a_person(&mut tx, request.handle.as_str()).await? {
        return Err(name_taken());
    }
    let inserted = sqlx::query(
        "INSERT INTO user_groups (id, workspace_id, handle, name, created_by) VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (workspace_id, handle) DO NOTHING",
    )
    .bind(id.as_uuid())
    .bind(workspace.as_uuid())
    .bind(request.handle.as_str())
    .bind(request.name.as_str())
    .bind(actor.as_uuid())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        return Err(name_taken());
    }
    insert_members(&mut tx, id, &members).await?;
    let group = load_group(&mut tx, id)
        .await?
        .ok_or_else(|| AppError::internal("group vanished"))?;
    announce_change(&mut tx, workspace, &group).await?;
    tx.commit().await?;
    Ok(group)
}

async fn insert_members(
    conn: &mut PgConnection,
    group: GroupId,
    members: &[UserId],
) -> Result<(), AppError> {
    if members.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = members.iter().map(|user| user.as_uuid()).collect();
    sqlx::query(
        "INSERT INTO user_group_members (group_id, user_id) SELECT $1, u FROM unnest($2::uuid[]) AS u \
         ON CONFLICT DO NOTHING",
    )
    .bind(group.as_uuid())
    .bind(&ids)
    .execute(conn)
    .await?;
    Ok(())
}

async fn announce_change(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    group: &UserGroup,
) -> Result<(), AppError> {
    events::append_all(
        conn,
        workspace,
        vec![PendingEvent {
            kind: "group_changed",
            conversation_id: None,
            operation_id: None,
            audience: Audience::Workspace,
            event: Event::GroupChanged {
                group: group.clone(),
            },
        }],
    )
    .await?;
    Ok(())
}

/// The workspace a group belongs to, or `None` if there is no such group.
async fn workspace_of(pool: &PgPool, id: GroupId) -> Result<Option<WorkspaceId>, AppError> {
    let workspace: Option<Uuid> =
        sqlx::query_scalar("SELECT workspace_id FROM user_groups WHERE id = $1")
            .bind(id.as_uuid())
            .fetch_optional(pool)
            .await?;
    Ok(workspace.map(WorkspaceId::from_uuid))
}

/// Changes a group's handle, name, and/or complete membership. Admins and owners only; a group in
/// a workspace the caller is not in is `not_found`. Changing nothing appends nothing.
pub async fn update_group(
    pool: &PgPool,
    actor: UserId,
    id: GroupId,
    request: UpdateGroupRequest,
) -> Result<UserGroup, AppError> {
    let workspace = workspace_of(pool, id).await?.ok_or_else(group_not_found)?;
    match accounts::workspace_role(pool, workspace, actor).await? {
        None => return Err(group_not_found()),
        Some(role) if !role.can_manage_groups() => {
            return Err(AppError::forbidden(
                "only admins and owners manage user groups",
            ));
        }
        Some(_) => {}
    }

    let mut tx = pool.begin().await?;
    let members = match &request.members {
        Some(members) => Some(checked_members(&mut tx, workspace, members).await?),
        None => None,
    };
    // Claiming a new handle: the same lock and check as creating one.
    if let Some(handle) = &request.handle {
        lock_handle(&mut tx, handle.as_str()).await?;
    }
    // The group row, excluding other changes to it and a deletion.
    let locked: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM user_groups WHERE id = $1 FOR NO KEY UPDATE")
            .bind(id.as_uuid())
            .fetch_optional(&mut *tx)
            .await?;
    if locked.is_none() {
        return Err(group_not_found());
    }
    let current = load_group(&mut tx, id).await?.ok_or_else(group_not_found)?;

    let handle_changes = request
        .handle
        .as_ref()
        .is_some_and(|handle| handle != &current.handle);
    if handle_changes {
        let handle = request
            .handle
            .as_ref()
            .map(|h| h.as_str())
            .unwrap_or_default();
        if handle_names_a_person(&mut tx, handle).await? {
            return Err(name_taken());
        }
    }
    let name_changes = request
        .name
        .as_ref()
        .is_some_and(|name| name != &current.name);
    let members_change = members.as_ref().is_some_and(|new| new != &current.members);
    if !(handle_changes || name_changes || members_change) {
        return Ok(current);
    }

    if handle_changes || name_changes {
        let updated = sqlx::query(
            "UPDATE user_groups SET handle = COALESCE($2, handle), name = COALESCE($3, name) WHERE id = $1",
        )
        .bind(id.as_uuid())
        .bind(handle_changes.then(|| request.handle.as_ref().map(|h| h.as_str().to_owned())).flatten())
        .bind(request.name.as_ref().map(|n| n.as_str().to_owned()))
        .execute(&mut *tx)
        .await;
        match updated {
            Ok(_) => {}
            Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
                return Err(name_taken());
            }
            Err(error) => return Err(error.into()),
        }
    }
    if let Some(new) = members.as_ref().filter(|_| members_change) {
        let keep: Vec<Uuid> = new.iter().map(|user| user.as_uuid()).collect();
        sqlx::query("DELETE FROM user_group_members WHERE group_id = $1 AND user_id <> ALL($2)")
            .bind(id.as_uuid())
            .bind(&keep)
            .execute(&mut *tx)
            .await?;
        insert_members(&mut tx, id, new).await?;
    }
    let group = load_group(&mut tx, id).await?.ok_or_else(group_not_found)?;
    announce_change(&mut tx, workspace, &group).await?;
    tx.commit().await?;
    Ok(group)
}

/// Deletes a group. Admins and owners only. Idempotent: a group that is already gone is a quiet
/// success (there is nothing left to authorize against), while a group in a workspace the caller
/// is not in is `not_found`.
pub async fn delete_group(pool: &PgPool, actor: UserId, id: GroupId) -> Result<(), AppError> {
    let Some(workspace) = workspace_of(pool, id).await? else {
        return Ok(());
    };
    match accounts::workspace_role(pool, workspace, actor).await? {
        None => return Err(group_not_found()),
        Some(role) if !role.can_manage_groups() => {
            return Err(AppError::forbidden(
                "only admins and owners manage user groups",
            ));
        }
        Some(_) => {}
    }

    let mut tx = pool.begin().await?;
    let locked: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM user_groups WHERE id = $1 FOR UPDATE")
            .bind(id.as_uuid())
            .fetch_optional(&mut *tx)
            .await?;
    if locked.is_none() {
        // Deleted while we were deciding; the work is done.
        return Ok(());
    }
    sqlx::query("DELETE FROM user_groups WHERE id = $1")
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await?;
    events::append_all(
        &mut tx,
        workspace,
        vec![PendingEvent {
            kind: "group_deleted",
            conversation_id: None,
            operation_id: None,
            audience: Audience::Workspace,
            event: Event::GroupDeleted { group_id: id },
        }],
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Whether `username` may be registered as far as the mention namespace goes: not a reserved word.
/// (A group handle is checked under the handle lock, inside the registration transaction.)
pub fn username_is_reserved(username: &str) -> bool {
    handle_is_reserved(username)
}
