//! Test helpers: a scripted server model and shortcuts for driving the core.
#![allow(dead_code)]

use std::collections::HashMap;

use lazy_client_core::{
    ClientCore, ContentCache, Effect, Input, Output, SessionPhase, StartupState, TimelineItem,
};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, DisplayName, EventSeq, MessageDocument,
    MessageId, OperationId, Timestamp, UserId, Username, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    BootstrapResponse, Conversation, Event, EventEnvelope, Message, PROTOCOL_VERSION, ServerFrame,
    User, Workspace, WorkspaceBootstrap,
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

/// A world with one workspace, one channel, and a signed-in person.
pub struct World {
    pub me: User,
    pub other: User,
    pub workspace: Workspace,
    pub channel: Conversation,
    pub second_channel: Conversation,
    pub head: EventSeq,
}

impl World {
    pub fn new() -> Self {
        let workspace = Workspace {
            id: WorkspaceId::generate(),
            name: "Lazy".into(),
        };
        let conversation = |name: &str| Conversation {
            id: ConversationId::generate(),
            workspace_id: workspace.id,
            name: ChannelName::parse(name).unwrap(),
            topic: None,
            visibility: ChannelVisibility::Public,
            archived: false,
        };
        let channel = conversation("general");
        let second_channel = conversation("random");
        Self {
            me: user("jules"),
            other: user("zoe"),
            workspace,
            channel,
            second_channel,
            head: seq(10),
        }
    }

    pub fn bootstrap(&self) -> BootstrapResponse {
        BootstrapResponse {
            protocol_version: PROTOCOL_VERSION,
            user: self.me.clone(),
            workspaces: vec![WorkspaceBootstrap {
                workspace: self.workspace.clone(),
                role: WorkspaceRole::Member,
                conversations: vec![self.channel.clone(), self.second_channel.clone()],
                members: vec![self.me.clone(), self.other.clone()],
                head: self.head,
            }],
        }
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
