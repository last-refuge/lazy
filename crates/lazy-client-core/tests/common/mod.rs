//! Test helpers: a scripted server model and shortcuts for driving the core.
#![allow(dead_code)]

use std::collections::HashMap;

use lazy_client_core::{
    ClientCore, ContentCache, Effect, Input, Output, SessionPhase, StartupState, TimelineItem,
};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
    MessageDocument, MessageId, OperationId, Timestamp, UserId, Username, WorkspaceId,
    WorkspaceRole,
};
use lazy_protocol::{
    BootstrapResponse, Conversation, ConversationUserState, Event, EventEnvelope, HistoryPage,
    Message, PROTOCOL_VERSION, ServerFrame, User, Workspace, WorkspaceBootstrap,
};

pub fn user(name: &str) -> User {
    User {
        id: UserId::generate(),
        username: Username::parse(name).unwrap(),
        display_name: DisplayName::parse(&name.to_uppercase()).unwrap(),
    }
}

pub fn seq(value: i64) -> EventSeq {
    EventSeq::new(value).unwrap()
}

/// A public channel the person belongs to.
pub fn channel(workspace: WorkspaceId, name: &str) -> Conversation {
    Conversation {
        id: ConversationId::generate(),
        workspace_id: workspace,
        kind: ConversationKind::Channel,
        name: Some(ChannelName::parse(name).unwrap()),
        topic: None,
        description: None,
        visibility: ChannelVisibility::Public,
        archived: false,
        is_default: false,
        created_by: None,
        participants: Vec::new(),
        member_count: 2,
    }
}

/// A direct (two people) or group (more) conversation.
pub fn people(workspace: WorkspaceId, participants: &[&User]) -> Conversation {
    let mut ids: Vec<UserId> = participants.iter().map(|u| u.id).collect();
    ids.sort();
    Conversation {
        id: ConversationId::generate(),
        workspace_id: workspace,
        kind: if ids.len() == 2 {
            ConversationKind::Direct
        } else {
            ConversationKind::Group
        },
        name: None,
        topic: None,
        description: None,
        visibility: ChannelVisibility::Private,
        archived: false,
        is_default: false,
        created_by: None,
        participants: ids.clone(),
        member_count: u32::try_from(ids.len()).unwrap(),
    }
}

/// A read state with no favorite flag.
pub fn read_state(
    conversation: ConversationId,
    last_read: i64,
    unread: u32,
) -> ConversationUserState {
    ConversationUserState {
        conversation_id: conversation,
        favorite: false,
        last_read: seq(last_read),
        unread_count: unread,
        mention_count: 0,
    }
}

/// A world with one workspace, one channel, and a signed-in person.
pub struct World {
    pub me: User,
    pub other: User,
    pub workspace: Workspace,
    pub channel: Conversation,
    pub second_channel: Conversation,
    pub head: EventSeq,
    /// The person's read state, as the bootstrap reports it. Empty means the core starts every
    /// conversation unread-free.
    pub user_states: Vec<ConversationUserState>,
    /// The person's role in the workspace.
    pub role: WorkspaceRole,
    /// More conversations the person belongs to, after the two channels.
    pub extra: Vec<Conversation>,
    /// More people in the workspace, after `me` and `other`.
    pub extra_users: Vec<User>,
}

impl World {
    pub fn new() -> Self {
        let workspace = Workspace {
            id: WorkspaceId::generate(),
            name: "Lazy".into(),
        };
        let mut channel = channel(workspace.id, "general");
        channel.is_default = true;
        let second_channel = self::channel(workspace.id, "random");
        Self {
            me: user("jules"),
            other: user("zoe"),
            workspace,
            channel,
            second_channel,
            head: seq(10),
            user_states: Vec::new(),
            role: WorkspaceRole::Member,
            extra: Vec::new(),
            extra_users: Vec::new(),
        }
    }

    pub fn bootstrap(&self) -> BootstrapResponse {
        BootstrapResponse {
            protocol_version: PROTOCOL_VERSION,
            user: self.me.clone(),
            workspaces: vec![WorkspaceBootstrap {
                workspace: self.workspace.clone(),
                role: self.role,
                conversations: self.conversations(),
                user_state: self.user_states.clone(),
                members: self.people(),
                head: self.head,
            }],
        }
    }

    /// Every conversation the person belongs to.
    pub fn conversations(&self) -> Vec<Conversation> {
        let mut all = vec![self.channel.clone(), self.second_channel.clone()];
        all.extend(self.extra.iter().cloned());
        all
    }

    /// Everyone in the workspace.
    pub fn people(&self) -> Vec<User> {
        let mut all = vec![self.me.clone(), self.other.clone()];
        all.extend(self.extra_users.iter().cloned());
        all
    }

    /// Wraps any event in an envelope at position `at`.
    pub fn event_at(&self, at: i64, event: Event) -> Input {
        Input::Frame(ServerFrame::Event(EventEnvelope {
            workspace_id: self.workspace.id,
            seq: seq(at),
            occurred_at: Timestamp::now(),
            event,
        }))
    }

    pub fn message(
        &self,
        conversation: ConversationId,
        author: &User,
        at: i64,
        text: &str,
    ) -> Message {
        Message {
            id: MessageId::generate(),
            conversation_id: conversation,
            author_id: author.id,
            document: MessageDocument::from_plain_text(text).unwrap(),
            created_at: Timestamp::now(),
            revision: 1,
            seq: seq(at),
        }
    }

    pub fn event(&self, message: Message, operation_id: Option<OperationId>) -> Input {
        let at = message.seq;
        Input::Frame(ServerFrame::Event(EventEnvelope {
            workspace_id: self.workspace.id,
            seq: at,
            occurred_at: Timestamp::now(),
            event: Event::MessageCreated {
                message,
                operation_id,
            },
        }))
    }
}

/// Drives a core and records every effect it emits.
pub struct Driver {
    pub core: ClientCore,
    pub effects: Vec<Effect>,
    pub changes: Vec<lazy_client_core::Change>,
}

impl Driver {
    pub fn new() -> Self {
        Self {
            core: ClientCore::new(),
            effects: Vec::new(),
            changes: Vec::new(),
        }
    }

    pub fn send(&mut self, input: Input) -> Output {
        let out = self.core.handle(input);
        self.effects.extend(out.effects.iter().cloned());
        self.changes.extend(out.changes.iter().copied());
        out
    }

    /// Starts with a restored session and completes bootstrap.
    pub fn ready(world: &World) -> Self {
        let mut driver = Self::new();
        driver.send(Input::Started(StartupState {
            session_restored: true,
            ..Default::default()
        }));
        driver.send(Input::BootstrapLoaded(Box::new(world.bootstrap())));
        assert_eq!(driver.core.phase(), &SessionPhase::Ready);
        driver
    }

    /// Answers the latest history request for `conversation` with a page.
    pub fn load_history(
        &mut self,
        conversation: ConversationId,
        messages: Vec<Message>,
        has_more: bool,
    ) {
        let generation = self
            .effects
            .iter()
            .rev()
            .find_map(|effect| match effect {
                Effect::FetchHistory {
                    conversation: c,
                    generation,
                    ..
                } if *c == conversation => Some(*generation),
                _ => None,
            })
            .expect("history was requested for the conversation");
        self.send(Input::HistoryLoaded {
            conversation,
            generation,
            result: Ok(HistoryPage { messages, has_more }),
        });
    }

    /// Every mark-read request, oldest first.
    pub fn mark_reads(&self) -> Vec<(ConversationId, i64)> {
        self.effects_matching(|e| match e {
            Effect::MarkRead {
                conversation,
                up_to,
            } => Some((*conversation, up_to.get())),
            _ => None,
        })
    }

    /// The conversations history was requested for, oldest first.
    pub fn history_requests(&self) -> Vec<ConversationId> {
        self.effects_matching(|e| match e {
            Effect::FetchHistory { conversation, .. } => Some(*conversation),
            _ => None,
        })
    }

    /// Every cache the core asked to be saved, oldest first.
    pub fn caches(&self) -> Vec<&ContentCache> {
        self.effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::PersistCache(cache) => Some(cache.as_ref()),
                _ => None,
            })
            .collect()
    }

    pub fn last_cache(&self) -> Option<&ContentCache> {
        self.caches().pop()
    }

    pub fn effects_matching<T>(&self, pick: impl Fn(&Effect) -> Option<T>) -> Vec<T> {
        self.effects.iter().filter_map(pick).collect()
    }

    pub fn timeline(&self, conversation: ConversationId) -> &[TimelineItem] {
        self.core.timeline(conversation)
    }
}

pub fn message_ids(items: &[TimelineItem]) -> Vec<MessageId> {
    items.iter().filter_map(TimelineItem::message_id).collect()
}

pub fn count_by_key<K: std::hash::Hash + Eq>(
    keys: impl IntoIterator<Item = K>,
) -> HashMap<K, usize> {
    let mut counts = HashMap::new();
    for key in keys {
        *counts.entry(key).or_insert(0) += 1;
    }
    counts
}
