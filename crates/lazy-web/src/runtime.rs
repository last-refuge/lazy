//! The web client's effect executor: what the desktop runtime (`lazy-client-runtime`) does for the
//! core, done with the browser's `fetch`, WebSocket, and `localStorage`.
//!
//! Requests follow the desktop runtime's rules exactly (which are retried, which are tried once and
//! reconciled by the core), because the core relies on them. What the web client does not offer
//! (drop-ins, integrations, workspace administration) is never asked for by its screens; if the
//! core asks anyway, the effect is answered with a refusal where it waits for an answer, so nothing
//! hangs.
//!
//! Files: a file chosen in the browser is read into memory when attached (the "outbox" of the
//! desktop client), uploaded from there, and forgotten when the message is sent or the file
//! removed. A page that is closed loses them; the core then reports the file missing. Downloads
//! and pictures are plain links to the server, which the cookie authenticates, so the effects that
//! fetch them for the desktop client are not used.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use lazy_client_core::{
    ActionKind, ActionOutcome, Effect, Input, RequestError, ServerFacts, StagedFile, StartupState,
    TransferError,
};
use lazy_domain::notification::NotificationPermission;
use lazy_domain::{AttachmentId, WorkspaceId};
use lazy_protocol::api::paths;
use lazy_protocol::{
    ApiError, BootstrapResponse, BrowseChannelsResponse, ConversationMembersResponse,
    ConversationResponse, ErrorCode, HistoryPage, HistoryQuery, InboxPage, InboxReadResponse,
    MarkReadRequest, MarkThreadReadRequest, MessageResponse, NotificationSettingsResponse,
    OperationStatus, PeoplePage, PersonResponse, SavedPage, SavedResponse, SendMessageRequest,
    SendMessageResponse, SetFavoriteRequest, SetThreadFollowRequest, StatusResponse,
    ThreadStateResponse, UserStateResponse, WebSessionResponse,
};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

use crate::api;
use crate::realtime::{self, Subscription};
use crate::store;

/// Where results go: the host's input queue.
pub(crate) type Emit = Rc<dyn Fn(Input)>;

/// The reads of which only the newest is wanted (a search, a directory page).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Slot {
    Search,
    Inbox,
    Saved,
    People,
    Profile,
    Browse,
}

pub(crate) struct Runtime {
    emit: Emit,
    subscriptions: RefCell<HashMap<WorkspaceId, Subscription>>,
    /// The bytes of files attached but not yet sent, with the generation of their copy.
    files: RefCell<HashMap<AttachmentId, Vec<u8>>>,
    /// The upload of each file that is wanted, by generation; a cancelled one reports nothing.
    uploads: RefCell<HashMap<AttachmentId, u64>>,
    /// The generation of each single read that is wanted.
    slots: RefCell<HashMap<Slot, u64>>,
    /// Bumped on sign-out, so answers to the person who left are dropped.
    epoch: Cell<u64>,
    /// Notifications on screen, by tag, so a read conversation can take its own back.
    notifications: RefCell<HashMap<String, (lazy_domain::ConversationId, web_sys::Notification)>>,
}

impl Runtime {
    pub(crate) fn new(emit: Emit) -> Rc<Runtime> {
        Rc::new(Runtime {
            emit,
            subscriptions: RefCell::new(HashMap::new()),
            files: RefCell::new(HashMap::new()),
            uploads: RefCell::new(HashMap::new()),
            slots: RefCell::new(HashMap::new()),
            epoch: Cell::new(0),
            notifications: RefCell::new(HashMap::new()),
        })
    }

    /// What the browser holds for the core's first input.
    pub(crate) fn startup_state(&self) -> StartupState {
        let mut problems = Vec::new();
        if !store::available() {
            problems.push(
                "This browser is not keeping data for Lazy (a private window, or storage is turned \
                 off): drafts and unsent messages will not survive reloading the page."
                    .to_owned(),
            );
        }
        let (journal, journal_problem) = store::journal();
        problems.extend(journal_problem);
        StartupState {
            session_restored: store::signed_in(),
            journal,
            drafts: store::drafts(),
            thread_drafts: store::thread_drafts(),
            view: store::view(),
            cache: None,
            emoji_prefs: store::emoji_prefs(),
            recent_searches: store::recent_searches(),
            notification_prompts: store::prompt_history(),
            recent_conversations: store::recent_conversations(),
            appearance: Default::default(),
            device_choices: Default::default(),
            active_workspace: store::active_workspace(),
            problems,
            server: lazy_client_core::ServerInfo {
                url: origin(),
                source: lazy_client_core::ServerSource::Environment,
                trying: None,
                check: lazy_client_core::ServerCheck::Unchecked,
            },
        }
    }

    /// Runs `task` and emits its input, unless the person signed out meanwhile.
    fn spawn(self: &Rc<Self>, task: impl std::future::Future<Output = Input> + 'static) {
        let this = self.clone();
        let epoch = self.epoch.get();
        spawn_local(async move {
            let input = task.await;
            if this.epoch.get() == epoch {
                (this.emit)(input);
            }
        });
    }

    /// Runs a read of which only the newest is wanted.
    fn single_read(
        self: &Rc<Self>,
        slot: Slot,
        generation: u64,
        task: impl std::future::Future<Output = Input> + 'static,
    ) {
        self.slots.borrow_mut().insert(slot, generation);
        let this = self.clone();
        let epoch = self.epoch.get();
        spawn_local(async move {
            let input = task.await;
            let current = this.slots.borrow().get(&slot) == Some(&generation);
            if current && this.epoch.get() == epoch {
                this.slots.borrow_mut().remove(&slot);
                (this.emit)(input);
            }
        });
    }

    fn cancel(&self, slot: Slot) {
        self.slots.borrow_mut().remove(&slot);
    }

    fn stop_subscriptions(&self) {
        for (_, subscription) in self.subscriptions.borrow_mut().drain() {
            subscription.stop();
        }
    }

    /// Answers an effect the web client does not carry out.
    fn unsupported(&self, what: &str) {
        web_sys::console::warn_1(&JsValue::from_str(&format!(
            "Lazy on the web does not do this: {what}"
        )));
    }

    pub(crate) fn execute(self: &Rc<Self>, effect: Effect) {
        match effect {
            Effect::Register(request) => self.spawn(async move {
                authenticated(
                    api::post::<WebSessionResponse, _>(paths::WEB_REGISTER, &request).await,
                )
            }),
            Effect::Login(request) => self.spawn(async move {
                authenticated(api::post::<WebSessionResponse, _>(paths::WEB_LOGIN, &request).await)
            }),
            Effect::SignOut => {
                self.epoch.set(self.epoch.get() + 1);
                self.stop_subscriptions();
                self.files.borrow_mut().clear();
                self.uploads.borrow_mut().clear();
                self.slots.borrow_mut().clear();
                self.clear_all_notifications();
                store::wipe();
                spawn_local(async {
                    // Best effort: the server expires the session anyway.
                    let _ = api::post_unit(paths::WEB_LOGOUT).await;
                });
            }
            Effect::ForgetSession => {
                self.stop_subscriptions();
                store::set_signed_in(false);
            }
            Effect::CheckServer { url, generation } => {
                let this = self.clone();
                let epoch = self.epoch.get();
                spawn_local(async move {
                    let result = api::get::<lazy_protocol::HealthResponse>(paths::HEALTH)
                        .await
                        .map(|health| health.protocol_version)
                        .map_err(|_| "the server did not answer".to_owned());
                    // What the server says about taking accounts comes just before the answer,
                    // as from the desktop runtime (D29).
                    let facts = match result {
                        Ok(_) => api::get::<lazy_protocol::ServerInfoResponse>(paths::SERVER_INFO)
                            .await
                            .ok()
                            .map(|info| ServerFacts {
                                registration: info.registration,
                                setup_code_required: info.setup_code_required,
                            }),
                        Err(_) => None,
                    };
                    if this.epoch.get() != epoch {
                        return;
                    }
                    if result.is_ok() {
                        (this.emit)(Input::ServerFacts { url, facts });
                    }
                    (this.emit)(Input::ServerChecked { generation, result });
                });
            }
            Effect::UseServer { generation, .. } => {
                // The web client always uses the server that served it.
                self.spawn(async move {
                    Input::ServerSwitched {
                        generation,
                        result: Err("the web client uses the server it came from".to_owned()),
                    }
                });
            }
            Effect::PreviewInvitation {
                request,
                generation,
            } => self.spawn(async move {
                let result = api::post::<lazy_protocol::InvitationPreview, _>(
                    paths::PREVIEW_INVITATION,
                    &request,
                )
                .await
                .map_err(preview_failure);
                Input::InvitationPreviewed { generation, result }
            }),
            Effect::LoadBootstrap => self.spawn(async {
                match api::retry_read(|| api::get::<BootstrapResponse>(paths::BOOTSTRAP)).await {
                    Ok(response) => Input::BootstrapLoaded(Box::new(response)),
                    Err(error) => Input::BootstrapFailed(error),
                }
            }),
            Effect::Subscribe { workspace, after } => {
                let mut subscriptions = self.subscriptions.borrow_mut();
                if let Some(existing) = subscriptions.get(&workspace)
                    && existing.resubscribe(after)
                {
                    return;
                }
                let subscription = realtime::start(workspace, after, self.emit.clone());
                subscriptions.insert(workspace, subscription);
            }
            Effect::Unsubscribe { workspace } => {
                if let Some(subscription) = self.subscriptions.borrow_mut().remove(&workspace) {
                    subscription.stop();
                }
            }
            Effect::FetchHistory {
                conversation,
                before,
                limit,
                generation,
            } => self.spawn(async move {
                let path = api::with_query(
                    &paths::messages(conversation),
                    &HistoryQuery { before, limit },
                );
                let result = api::retry_read(|| api::get::<HistoryPage>(&path)).await;
                Input::HistoryLoaded {
                    conversation,
                    generation,
                    result,
                }
            }),
            Effect::FetchReplies {
                root,
                before,
                limit,
                generation,
            } => self.spawn(async move {
                let path = api::with_query(&paths::replies(root), &HistoryQuery { before, limit });
                let result = api::retry_read(|| api::get::<HistoryPage>(&path)).await;
                Input::RepliesLoaded {
                    root,
                    generation,
                    result,
                }
            }),
            Effect::SetThreadFollow { root, following } => self.spawn(async move {
                let path = paths::thread_action(root, "follow");
                let result = api::put::<ThreadStateResponse, _>(
                    &path,
                    &SetThreadFollowRequest { following },
                )
                .await;
                Input::ThreadFollowResult { root, result }
            }),
            Effect::MarkThreadRead { root, up_to } => self.spawn(async move {
                let path = paths::thread_action(root, "read");
                let result =
                    api::post::<ThreadStateResponse, _>(&path, &MarkThreadReadRequest { up_to })
                        .await;
                Input::ThreadReadResult {
                    root,
                    up_to,
                    result,
                }
            }),
            Effect::PersistThreadDrafts(drafts) => store::save_thread_drafts(&drafts),
            Effect::PersistSubmission(entry) => {
                let operation = entry.operation_id;
                let saved = store::journal_add(entry);
                self.spawn(async move {
                    if saved {
                        Input::SubmissionPersisted { operation }
                    } else {
                        Input::SubmissionPersistFailed {
                            operation,
                            message: "this browser would not save the message".to_owned(),
                        }
                    }
                });
            }
            Effect::SettleSubmission { operation } => store::journal_settle(operation),
            Effect::SendMessage {
                operation,
                conversation,
                document,
                thread_root,
                also_in_channel,
                attachments,
            } => {
                // The files are on the server now.
                for id in &attachments {
                    self.files.borrow_mut().remove(id);
                }
                self.spawn(async move {
                    let request = SendMessageRequest {
                        operation_id: operation,
                        document,
                        thread_root,
                        also_in_channel,
                        attachment_ids: attachments,
                    };
                    // Exactly one attempt: the core reconciles an unknown outcome.
                    let result = api::post::<SendMessageResponse, _>(
                        &paths::messages(conversation),
                        &request,
                    )
                    .await;
                    Input::SendResult { operation, result }
                });
            }
            Effect::EditMessage { message, request } => self.spawn(async move {
                let operation = request.operation_id;
                let result =
                    api::patch::<MessageResponse, _>(&paths::message(message), &request).await;
                Input::EditResult { operation, result }
            }),
            Effect::DeleteMessage { message } => self.spawn(async move {
                let path = paths::message(message);
                let result = api::retry_read(|| api::delete::<MessageResponse>(&path)).await;
                Input::DeleteResult { message, result }
            }),
            Effect::FetchMessage { message } => self.spawn(async move {
                let path = paths::message(message);
                let result = api::retry_read(|| api::get::<MessageResponse>(&path)).await;
                Input::MessageFetched { message, result }
            }),
            Effect::QueryOperationStatus { operation } => self.spawn(async move {
                let path = paths::operation(operation);
                let result = api::retry_read(|| api::get::<OperationStatus>(&path)).await;
                Input::OperationStatusResult { operation, result }
            }),
            Effect::FetchMembers {
                conversation,
                generation,
            } => self.spawn(async move {
                let path = paths::conversation_action(conversation, "members");
                let result =
                    api::retry_read(|| api::get::<ConversationMembersResponse>(&path)).await;
                Input::MembersFetched {
                    conversation,
                    generation,
                    result,
                }
            }),
            Effect::SetReaction { message, request } => self.spawn(async move {
                let emoji = request.emoji;
                let result =
                    api::put::<MessageResponse, _>(&paths::reactions(message), &request).await;
                Input::ReactionResult {
                    message,
                    emoji,
                    result,
                }
            }),
            Effect::PersistDrafts(drafts) => store::save_drafts(&drafts),
            Effect::PersistView(view) => store::save_view(&view),
            Effect::PersistEmojiPrefs(prefs) => store::save_emoji_prefs(&prefs),
            Effect::PersistRecentSearches(queries) => store::save_recent_searches(&queries),
            Effect::PersistRecentConversations(ids) => store::save_recent_conversations(&ids),
            Effect::PersistActiveWorkspace(workspace) => store::save_active_workspace(workspace),
            Effect::PersistNotificationPrompts(history) => store::save_prompt_history(&history),
            // The web client keeps no copy of the messages: it loads them from the server.
            Effect::PersistCache(_) | Effect::DiscardCache => {}
            Effect::PersistAppearance(_) | Effect::PersistDeviceChoices(_) => {}
            Effect::Search {
                workspace,
                generation,
                params,
            } => self.single_read(Slot::Search, generation, async move {
                let path = api::with_query(&paths::search(workspace), &params);
                let result = api::retry_read(|| async {
                    api::decode(
                        api::send("GET", &path, api::Body::None, api::TIMEOUT).await,
                        api::classify_search_failure,
                    )
                })
                .await;
                Input::SearchResult { generation, result }
            }),
            Effect::CancelSearch => self.cancel(Slot::Search),
            Effect::LoadInbox {
                workspace,
                generation,
                query,
            } => self.single_read(Slot::Inbox, generation, async move {
                let path = api::with_query(&paths::inbox(workspace), &query);
                let result = api::retry_read(|| api::get::<InboxPage>(&path)).await;
                Input::InboxLoaded { generation, result }
            }),
            Effect::CancelInbox => self.cancel(Slot::Inbox),
            Effect::MarkInboxRead { workspace } => self.spawn(async move {
                let result =
                    api::post_empty::<InboxReadResponse>(&paths::inbox_read(workspace)).await;
                Input::InboxReadResult { result }
            }),
            Effect::SetMessageSaved { message, request } => self.spawn(async move {
                let result =
                    api::put::<SavedResponse, _>(&paths::message_saved(message), &request).await;
                Input::MessageSavedResult {
                    message_id: message,
                    result,
                }
            }),
            Effect::LoadSaved {
                workspace,
                generation,
                query,
            } => self.single_read(Slot::Saved, generation, async move {
                let path = api::with_query(&paths::saved(workspace), &query);
                let result = api::retry_read(|| api::get::<SavedPage>(&path)).await;
                Input::SavedLoaded { generation, result }
            }),
            Effect::CancelSaved => self.cancel(Slot::Saved),
            Effect::LoadPeople {
                workspace,
                generation,
                query,
            } => self.single_read(Slot::People, generation, async move {
                let path = api::with_query(&paths::people(workspace), &query);
                let result = api::retry_read(|| api::get::<PeoplePage>(&path)).await;
                Input::PeopleLoaded { generation, result }
            }),
            Effect::CancelPeople => self.cancel(Slot::People),
            Effect::LoadProfile {
                workspace,
                generation,
                user,
            } => self.single_read(Slot::Profile, generation, async move {
                let path = paths::person(workspace, user);
                let result = api::retry_read(|| api::get::<PersonResponse>(&path)).await;
                Input::ProfileLoaded { generation, result }
            }),
            Effect::CancelProfile => self.cancel(Slot::Profile),
            Effect::LoadBrowse {
                workspace,
                generation,
                query,
            } => self.single_read(Slot::Browse, generation, async move {
                let path = api::with_query(&paths::browse_channels(workspace), &query);
                let result = api::retry_read(|| api::get::<BrowseChannelsResponse>(&path)).await;
                Input::BrowseLoaded { generation, result }
            }),
            Effect::CancelBrowse => self.cancel(Slot::Browse),
            Effect::CreateChannel { workspace, request } => self.spawn(async move {
                let operation = request.operation_id;
                let result =
                    api::post::<ConversationResponse, _>(&paths::channels(workspace), &request)
                        .await;
                Input::ConversationCommandResult { operation, result }
            }),
            Effect::OpenConversation { workspace, request } => self.spawn(async move {
                let operation = request.operation_id;
                let result = api::post::<ConversationResponse, _>(
                    &paths::open_conversation(workspace),
                    &request,
                )
                .await;
                Input::ConversationCommandResult { operation, result }
            }),
            Effect::JoinChannel { conversation } => self.spawn(async move {
                let path = paths::conversation_action(conversation, "join");
                let result = api::post_empty::<ConversationResponse>(&path)
                    .await
                    .map(|r| ActionOutcome::Conversation(r.conversation));
                Input::ActionResult {
                    kind: ActionKind::Join(conversation),
                    result,
                }
            }),
            Effect::LeaveChannel { conversation } => self.spawn(async move {
                let path = paths::conversation_action(conversation, "leave");
                let result = api::post_unit(&path).await.map(|()| ActionOutcome::Done);
                Input::ActionResult {
                    kind: ActionKind::Leave(conversation),
                    result,
                }
            }),
            Effect::AddMembers {
                conversation,
                request,
            } => self.spawn(async move {
                let path = paths::conversation_action(conversation, "members");
                let result = api::post::<ConversationResponse, _>(&path, &request)
                    .await
                    .map(|r| ActionOutcome::Conversation(r.conversation));
                Input::ActionResult {
                    kind: ActionKind::AddMembers(conversation),
                    result,
                }
            }),
            Effect::RemoveMember { conversation, user } => self.spawn(async move {
                let result = api::delete_unit(&paths::member(conversation, user))
                    .await
                    .map(|()| ActionOutcome::Done);
                Input::ActionResult {
                    kind: ActionKind::RemoveMember(conversation, user),
                    result,
                }
            }),
            Effect::UpdateConversation {
                conversation,
                request,
            } => self.spawn(async move {
                let result = api::patch::<ConversationResponse, _>(
                    &paths::conversation(conversation),
                    &request,
                )
                .await
                .map(|r| ActionOutcome::Conversation(r.conversation));
                Input::ActionResult {
                    kind: ActionKind::UpdateConversation(conversation),
                    result,
                }
            }),
            Effect::ArchiveConversation { conversation } => self.spawn(async move {
                let path = paths::conversation_action(conversation, "archive");
                let result = api::post_empty::<ConversationResponse>(&path)
                    .await
                    .map(|r| ActionOutcome::Conversation(r.conversation));
                Input::ActionResult {
                    kind: ActionKind::Archive(conversation),
                    result,
                }
            }),
            Effect::UnarchiveConversation { conversation } => self.spawn(async move {
                let path = paths::conversation_action(conversation, "unarchive");
                let result = api::post_empty::<ConversationResponse>(&path)
                    .await
                    .map(|r| ActionOutcome::Conversation(r.conversation));
                Input::ActionResult {
                    kind: ActionKind::Unarchive(conversation),
                    result,
                }
            }),
            Effect::SetFavorite {
                conversation,
                favorite,
            } => self.spawn(async move {
                let path = paths::conversation_action(conversation, "favorite");
                let result =
                    api::put::<UserStateResponse, _>(&path, &SetFavoriteRequest { favorite })
                        .await
                        .map(|r| ActionOutcome::UserState(r.state));
                Input::ActionResult {
                    kind: ActionKind::SetFavorite(conversation),
                    result,
                }
            }),
            Effect::MarkRead {
                conversation,
                up_to,
            } => self.spawn(async move {
                let path = paths::conversation_action(conversation, "read");
                let result = api::post::<UserStateResponse, _>(&path, &MarkReadRequest { up_to })
                    .await
                    .map(|r| r.state);
                Input::MarkReadResult {
                    conversation,
                    up_to,
                    result,
                }
            }),
            Effect::SetNotificationSettings {
                workspace,
                settings,
            } => self.spawn(async move {
                let path = paths::notification_settings(workspace);
                let result = api::put::<NotificationSettingsResponse, _>(&path, &settings).await;
                Input::NotificationSettingsResult { workspace, result }
            }),
            Effect::SetConversationNotificationPrefs {
                conversation,
                request,
            } => self.spawn(async move {
                let path = paths::conversation_notification_prefs(conversation);
                let result = api::put::<lazy_protocol::ConversationNotificationPrefsResponse, _>(
                    &path, &request,
                )
                .await;
                Input::ConversationPrefsResult {
                    conversation,
                    result,
                }
            }),
            Effect::SetStatus { workspace, request } => self.spawn(async move {
                let result =
                    api::put::<StatusResponse, _>(&paths::status(workspace), &request).await;
                Input::StatusResult { workspace, result }
            }),
            Effect::AcceptInvitation { request } => self.spawn(async move {
                // One attempt: an invitation's use is counted.
                let result = api::post::<lazy_protocol::JoinedWorkspace, _>(
                    paths::ACCEPT_INVITATION_ROUTE,
                    &request,
                )
                .await
                .map(|joined| ActionOutcome::Joined(Box::new(joined.workspace)));
                Input::ActionResult {
                    kind: ActionKind::AcceptInvitation,
                    result,
                }
            }),
            Effect::UpdateProfile { request } => self.spawn(async move {
                let result = api::retry_read(|| {
                    api::patch::<lazy_protocol::ProfileResponse, _>(paths::ME_ROUTE, &request)
                })
                .await
                .map(|response| ActionOutcome::Profile(response.user));
                Input::ActionResult {
                    kind: ActionKind::UpdateProfile,
                    result,
                }
            }),
            Effect::ChangePassword { request } => self.spawn(async move {
                // One attempt, as from the desktop runtime.
                let answer = api::send(
                    "PUT",
                    paths::ME_PASSWORD_ROUTE,
                    api::Body::Json(serde_json::to_string(&request).unwrap_or_default()),
                    api::TIMEOUT,
                )
                .await;
                Input::ActionResult {
                    kind: ActionKind::ChangePassword,
                    result: api::decode_unit(answer).map(|()| ActionOutcome::Done),
                }
            }),
            // ----- Files -----
            Effect::StageFile {
                attachment,
                generation,
                source,
            } => {
                let result = match source {
                    lazy_client_core::FileSource::Bytes(bytes) => {
                        let size = bytes.len() as u64;
                        self.files
                            .borrow_mut()
                            .insert(attachment, bytes.as_slice().to_vec());
                        Ok(StagedFile { size })
                    }
                    lazy_client_core::FileSource::Path(_) => Err(TransferError::Unreadable(
                        "not a file of this page".to_owned(),
                    )),
                };
                self.spawn(async move {
                    Input::FileStaged {
                        attachment,
                        generation,
                        result,
                    }
                });
            }
            Effect::UploadFile {
                attachment,
                conversation,
                generation,
                name,
                media_type,
                size,
            } => {
                self.uploads.borrow_mut().insert(attachment, generation);
                let bytes = self.files.borrow().get(&attachment).cloned();
                let this = self.clone();
                let epoch = self.epoch.get();
                spawn_local(async move {
                    let result = match bytes {
                        None => Err(TransferError::Missing),
                        Some(bytes) => {
                            let total = bytes.len() as u64;
                            let path = paths::attachment_upload(conversation, attachment);
                            let timeout = api::UPLOAD_TIMEOUT
                                + std::time::Duration::from_secs(size / (64 * 1024));
                            let answer = api::send(
                                "PUT",
                                &path,
                                api::Body::Bytes {
                                    bytes,
                                    content_type: media_type,
                                    headers: vec![(
                                        paths::ATTACHMENT_NAME_HEADER,
                                        api::percent_encode(&name),
                                    )],
                                },
                                timeout,
                            )
                            .await;
                            if answer
                                .as_ref()
                                .is_ok_and(|a| (200..300).contains(&a.status))
                            {
                                (this.emit)(Input::UploadProgress {
                                    attachment,
                                    generation,
                                    sent: total,
                                    total,
                                });
                            }
                            api::decode(answer, api::classify_failure)
                                .map_err(TransferError::Request)
                        }
                    };
                    let wanted = this.uploads.borrow().get(&attachment) == Some(&generation);
                    if wanted && this.epoch.get() == epoch {
                        this.uploads.borrow_mut().remove(&attachment);
                        (this.emit)(Input::UploadResult {
                            attachment,
                            generation,
                            result,
                        });
                    }
                });
            }
            Effect::CancelUpload { attachment } => {
                self.uploads.borrow_mut().remove(&attachment);
            }
            Effect::ReleaseFiles { attachments } | Effect::ForgetAttachments { attachments } => {
                for id in attachments {
                    self.files.borrow_mut().remove(&id);
                    self.uploads.borrow_mut().remove(&id);
                }
            }
            // Pictures and downloads are links the browser fetches itself.
            Effect::DownloadAttachment { .. }
            | Effect::CancelDownload { .. }
            | Effect::FetchImage { .. }
            | Effect::FetchAvatar { .. }
            | Effect::ForgetAvatars { .. }
            | Effect::FetchEmojiImage { .. }
            | Effect::ForgetEmojiImages { .. } => {}
            // ----- Notifications -----
            Effect::Notify(notification) => self.notify(notification),
            Effect::ClearNotifications { conversation } => {
                self.notifications.borrow_mut().retain(|_, (shown, note)| {
                    let keep = *shown != conversation;
                    if !keep {
                        note.close();
                    }
                    keep
                });
            }
            Effect::RetractNotification { .. } => {}
            Effect::SetBadge(count) => set_title_count(count),
            Effect::RefreshNotificationPermission => {
                let permission = notification_permission();
                self.spawn(async move { Input::NotificationPermissionChanged(permission) });
            }
            Effect::RequestNotificationPermission => {
                // Asked at once, inside the click that asked: browsers allow the question only
                // then.
                let asked = web_sys::Notification::request_permission().ok();
                let this = self.clone();
                spawn_local(async move {
                    if let Some(promise) = asked {
                        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
                    }
                    (this.emit)(Input::NotificationPermissionChanged(
                        notification_permission(),
                    ));
                });
            }
            Effect::OpenNotificationSettings => {}
            // ----- What the web client does not offer -----
            Effect::CreateWorkspace { .. } => self.refuse(ActionKind::CreateWorkspace),
            Effect::SetAvatar { .. } | Effect::RemoveAvatar => self.refuse(ActionKind::SetAvatar),
            other => self.unsupported(&format!("{other:?}").chars().take(60).collect::<String>()),
        }
    }

    /// Answers an action the web client does not carry out with a refusal, so it does not wait.
    fn refuse(self: &Rc<Self>, kind: ActionKind) {
        self.spawn(async move {
            Input::ActionResult {
                kind,
                result: Err(RequestError::Rejected(ApiError::new(
                    ErrorCode::Forbidden,
                    "Do this in the Lazy app.",
                ))),
            }
        });
    }

    fn notify(&self, notification: lazy_client_core::DesktopNotification) {
        if notification_permission() != NotificationPermission::Authorized {
            return;
        }
        let options = web_sys::NotificationOptions::new();
        options.set_body(&notification.body);
        options.set_tag(&notification.tag);
        let Ok(shown) = web_sys::Notification::new_with_options(&notification.title, &options)
        else {
            return;
        };
        let emit = self.emit.clone();
        let conversation = notification.conversation;
        let message = notification.message;
        let click = Closure::<dyn FnMut()>::new(move || {
            if let Some(window) = web_sys::window() {
                let _ = window.focus();
            }
            emit(Input::NotificationActivated {
                conversation,
                message,
            });
        });
        shown.set_onclick(Some(click.as_ref().unchecked_ref()));
        click.forget();
        if let Some((_, old)) = self
            .notifications
            .borrow_mut()
            .insert(notification.tag, (conversation, shown))
        {
            old.close();
        }
    }

    fn clear_all_notifications(&self) {
        for (_, (_, note)) in self.notifications.borrow_mut().drain() {
            note.close();
        }
    }
}

fn authenticated(result: Result<WebSessionResponse, RequestError>) -> Input {
    match result {
        Ok(session) => {
            store::set_signed_in(true);
            Input::Authenticated { user: session.user }
        }
        Err(error) => Input::AuthFailed(error),
    }
}

fn preview_failure(error: RequestError) -> lazy_client_core::PreviewFailure {
    use lazy_client_core::PreviewFailure;
    match error {
        RequestError::Rejected(rejected) => match rejected.code {
            ErrorCode::InvitationInvalid => PreviewFailure::Invalid,
            ErrorCode::Forbidden => PreviewFailure::Closed,
            ErrorCode::NotFound => PreviewFailure::Unsupported,
            _ => PreviewFailure::Failed("the server answered with an error".to_owned()),
        },
        RequestError::Unknown(detail) => PreviewFailure::Failed(detail),
    }
}

fn origin() -> String {
    web_sys::window()
        .and_then(|window| window.location().origin().ok())
        .unwrap_or_default()
}

pub(crate) fn notification_permission() -> NotificationPermission {
    let supported = web_sys::window()
        .map(|window| {
            js_sys::Reflect::has(&window, &JsValue::from_str("Notification")).unwrap_or(false)
        })
        .unwrap_or(false);
    if !supported {
        return NotificationPermission::Unavailable;
    }
    match web_sys::Notification::permission() {
        web_sys::NotificationPermission::Granted => NotificationPermission::Authorized,
        web_sys::NotificationPermission::Denied => NotificationPermission::Denied,
        _ => NotificationPermission::NotDetermined,
    }
}

fn set_title_count(count: u32) {
    if let Some(document) = web_sys::window().and_then(|window| window.document()) {
        document.set_title(&crate::view::title(count));
    }
}
