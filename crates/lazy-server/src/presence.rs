//! Presence (D45): who is online, meaning connected to this server's realtime socket and subscribed
//! to the workspace, on any device.
//!
//! Kept in memory only (this server runs as one process, `DEPLOY.md`): nothing is stored, and a
//! restart starts everyone offline until their clients reconnect, which they do within seconds.
//! Each workspace subscription holds an [`Online`] guard. The first one for a person announces
//! them online at once; when their last one ends they are announced offline only after
//! [`Presence`]'s grace period, so a client reconnecting after a network blip, or a new connection
//! replacing an old one, never flickers.
//!
//! Subscribers read who is online with [`Presence::watch`], which returns the people online now
//! and a receiver for every change after that, taken under one lock so nothing falls between.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lazy_domain::{UserId, WorkspaceId};
use tokio::sync::broadcast;

/// How long someone stays online after their last connection ends.
pub const GRACE: Duration = Duration::from_secs(15);

/// Changes queued per workspace for slow subscribers before they lag (and get a full list again).
const CHANGES_CAPACITY: usize = 256;

/// Someone came online (`true`) or went offline (`false`).
pub type Change = (UserId, bool);

/// Who is online, per workspace. Cheap to clone; every clone is the same registry.
#[derive(Clone)]
pub struct Presence {
    inner: Arc<Inner>,
}

struct Inner {
    workspaces: Mutex<HashMap<WorkspaceId, Workspace>>,
    grace: Duration,
}

struct Workspace {
    people: HashMap<UserId, Person>,
    changes: broadcast::Sender<Change>,
}

struct Person {
    connections: usize,
    /// Announced online and not yet announced offline.
    announced: bool,
    /// Bumped on every new connection, so a pending offline announcement from an earlier
    /// disconnect knows it is out of date.
    generation: u64,
}

impl Default for Presence {
    fn default() -> Self {
        Presence::new(GRACE)
    }
}

impl Presence {
    pub fn new(grace: Duration) -> Presence {
        Presence {
            inner: Arc::new(Inner {
                workspaces: Mutex::new(HashMap::new()),
                grace,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<WorkspaceId, Workspace>> {
        // A panic while holding this lock leaves counters, not invariants worth refusing over.
        self.inner
            .workspaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Marks `user` connected to `workspace` until the guard is dropped.
    pub fn connect(&self, workspace: WorkspaceId, user: UserId) -> Online {
        let mut workspaces = self.lock();
        let space = workspaces.entry(workspace).or_insert_with(|| Workspace {
            people: HashMap::new(),
            changes: broadcast::channel(CHANGES_CAPACITY).0,
        });
        let person = space.people.entry(user).or_insert(Person {
            connections: 0,
            announced: false,
            generation: 0,
        });
        person.connections += 1;
        person.generation += 1;
        if !person.announced {
            person.announced = true;
            let _ = space.changes.send((user, true));
        }
        Online {
            presence: self.clone(),
            workspace,
            user,
        }
    }

    /// The people online in `workspace` now, and every change after.
    pub fn watch(&self, workspace: WorkspaceId) -> (Vec<UserId>, broadcast::Receiver<Change>) {
        let mut workspaces = self.lock();
        let space = workspaces.entry(workspace).or_insert_with(|| Workspace {
            people: HashMap::new(),
            changes: broadcast::channel(CHANGES_CAPACITY).0,
        });
        (online_of(space), space.changes.subscribe())
    }

    /// The people online in `workspace` now.
    pub fn online(&self, workspace: WorkspaceId) -> Vec<UserId> {
        self.lock()
            .get(&workspace)
            .map(online_of)
            .unwrap_or_default()
    }

    fn disconnect(&self, workspace: WorkspaceId, user: UserId) {
        let generation = {
            let mut workspaces = self.lock();
            let Some(person) = workspaces
                .get_mut(&workspace)
                .and_then(|space| space.people.get_mut(&user))
            else {
                return;
            };
            person.connections = person.connections.saturating_sub(1);
            if person.connections > 0 {
                return;
            }
            person.generation
        };
        let presence = self.clone();
        let offline = move || presence.go_offline(workspace, user, generation);
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) if !self.inner.grace.is_zero() => {
                let grace = self.inner.grace;
                runtime.spawn(async move {
                    tokio::time::sleep(grace).await;
                    offline();
                });
            }
            _ => offline(),
        }
    }

    /// Announces `user` offline if nothing has connected them since `generation`.
    fn go_offline(&self, workspace: WorkspaceId, user: UserId, generation: u64) {
        let mut workspaces = self.lock();
        let Some(space) = workspaces.get_mut(&workspace) else {
            return;
        };
        let current = space
            .people
            .get(&user)
            .is_some_and(|person| person.connections == 0 && person.generation == generation);
        if current {
            let announced = space
                .people
                .remove(&user)
                .is_some_and(|person| person.announced);
            if announced {
                let _ = space.changes.send((user, false));
            }
        }
    }
}

fn online_of(space: &Workspace) -> Vec<UserId> {
    let mut online: Vec<UserId> = space
        .people
        .iter()
        .filter(|(_, person)| person.announced)
        .map(|(user, _)| *user)
        .collect();
    online.sort();
    online
}

/// One connection's claim that its person is online in a workspace; dropping it lets go.
pub struct Online {
    presence: Presence,
    workspace: WorkspaceId,
    user: UserId,
}

impl Drop for Online {
    fn drop(&mut self) {
        self.presence.disconnect(self.workspace, self.user);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn online_at_once_and_offline_after_the_grace_period() {
        let presence = Presence::new(Duration::from_secs(10));
        let workspace = WorkspaceId::generate();
        let ada = UserId::generate();
        let (online, mut changes) = presence.watch(workspace);
        assert!(online.is_empty());

        let first = presence.connect(workspace, ada);
        assert_eq!(changes.recv().await.unwrap(), (ada, true));
        // A second device changes nothing anyone sees.
        let second = presence.connect(workspace, ada);
        drop(first);
        assert_eq!(presence.online(workspace), vec![ada]);
        assert!(changes.try_recv().is_err());

        // The last one ends: still online through the grace period, then offline.
        drop(second);
        tokio::time::sleep(Duration::from_secs(9)).await;
        assert_eq!(presence.online(workspace), vec![ada]);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(changes.recv().await.unwrap(), (ada, false));
        assert!(presence.online(workspace).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn reconnecting_within_the_grace_period_never_goes_offline() {
        let presence = Presence::new(Duration::from_secs(10));
        let workspace = WorkspaceId::generate();
        let ada = UserId::generate();
        let (_, mut changes) = presence.watch(workspace);
        drop(presence.connect(workspace, ada));
        assert_eq!(changes.recv().await.unwrap(), (ada, true));
        tokio::time::sleep(Duration::from_secs(5)).await;
        let back = presence.connect(workspace, ada);
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert!(changes.try_recv().is_err(), "no offline, no second online");
        assert_eq!(presence.online(workspace), vec![ada]);
        drop(back);
        tokio::time::sleep(Duration::from_secs(11)).await;
        assert_eq!(changes.recv().await.unwrap(), (ada, false));
    }

    #[test]
    fn workspaces_are_separate_and_without_a_grace_period_offline_is_immediate() {
        let presence = Presence::new(Duration::ZERO);
        let (one, two) = (WorkspaceId::generate(), WorkspaceId::generate());
        let ada = UserId::generate();
        let online = presence.connect(one, ada);
        assert_eq!(presence.online(one), vec![ada]);
        assert!(presence.online(two).is_empty());
        drop(online);
        assert!(presence.online(one).is_empty());
    }
}
