//! Opening a permalink (`lazy://workspace/conversation/message`).
//!
//! Opening goes: check the workspace; if the message is already loaded, jump to it; otherwise
//! fetch it (which tells the core its conversation and position, or that it was deleted or may
//! not be read); select its conversation; then load older history a page at a time until the
//! message appears or [`PERMALINK_MAX_PAGES`] pages have been loaded for the job. The result is
//! `ClientCore::jump_target`, or a notice saying why there is nothing to show.
//!
//! A message in a public channel the person has not joined can be fetched but is not shown:
//! the client has no read-only view of a conversation, so the notice offers to join it
//! (`ClientCore::join_offer`), and the link opens by itself once they have.

use lazy_protocol::{ErrorCode, MessageResponse};

use super::conversations::readable_error;
use super::*;
use crate::model::PERMALINK_MAX_PAGES;

/// A permalink being opened.
#[derive(Debug)]
pub(super) struct Job {
    pub(super) link: Permalink,
    stage: Stage,
}

#[derive(Debug)]
enum Stage {
    /// The message is being fetched.
    Fetching,
    /// The conversation is selected and older history is being loaded until the message is found.
    Paging {
        conversation: ConversationId,
        /// The message's position, to tell "older than what is loaded" from "not there".
        seq: EventSeq,
        /// Older pages requested for this job so far.
        pages: u32,
    },
    /// The message is a reply: its thread is open and older replies are being loaded until it
    /// is found.
    PagingThread {
        conversation: ConversationId,
        root: MessageId,
        seq: EventSeq,
        pages: u32,
    },
}

impl ClientCore {
    pub(super) fn open_permalink(&mut self, link: Permalink, out: &mut Output) {
        if !matches!(self.phase, SessionPhase::Ready) {
            return;
        }
        self.permalink = None;
        self.set_jump(None, out);
        if self.join_offer.take().is_some() {
            self.set_notice(None, out);
        }
        if !self
            .workspaces
            .iter()
            .any(|info| info.workspace.id == link.workspace_id)
        {
            let notice = "That link is for a workspace you're not in.";
            return self.set_notice(Some(notice.to_owned()), out);
        }
        // A link into another workspace shows that workspace first.
        self.switch_workspace(link.workspace_id, out);
        // A message that is already loaded needs no request.
        if self.conversation(link.conversation_id).is_some()
            && let Some(row) = self.find_row(link.conversation_id, link.message_id)
        {
            let (key, root, seq) = (row.key, row.thread_root, row.seq());
            match (root, seq) {
                // A reply opens in its thread, even when the channel lists it too.
                (Some(root), Some(seq)) => {
                    return self.start_thread_job(link, link.conversation_id, root, seq, out);
                }
                (Some(_), None) => {}
                (None, _) => {
                    self.select_conversation(link.conversation_id, out);
                    return self.set_jump(
                        Some(JumpTarget {
                            conversation: link.conversation_id,
                            key,
                            thread: None,
                        }),
                        out,
                    );
                }
            }
        }
        self.permalink = Some(Job {
            link,
            stage: Stage::Fetching,
        });
        out.effects.push(Effect::FetchMessage {
            message: link.message_id,
        });
    }

    /// The key of the row showing `message`, if it is loaded in `conversation`.
    fn loaded_key(&self, conversation: ConversationId, message: MessageId) -> Option<ItemKey> {
        self.timelines
            .get(&conversation)?
            .message_row(message)
            .map(|row| row.key)
    }

    pub(super) fn permalink_fetched(
        &mut self,
        message: MessageId,
        result: Result<MessageResponse, RequestError>,
        out: &mut Output,
    ) {
        let Some(job) = &self.permalink else {
            return;
        };
        if job.link.message_id != message || !matches!(job.stage, Stage::Fetching) {
            return;
        }
        let link = job.link;
        self.permalink = None;
        let fetched = match result {
            Ok(response) => response.message,
            Err(error) if error.is_unauthenticated() => return self.session_expired(out),
            Err(RequestError::Rejected(error))
                if matches!(error.code, ErrorCode::NotFound | ErrorCode::Forbidden) =>
            {
                let notice = "That message doesn't exist, or you can't read it.";
                return self.set_notice(Some(notice.to_owned()), out);
            }
            Err(error) => {
                let notice = format!("Couldn't open that link. {}", readable_error(&error));
                return self.set_notice(Some(notice), out);
            }
        };
        if fetched.deleted {
            let notice = "That message was deleted.";
            return self.set_notice(Some(notice.to_owned()), out);
        }
        let conversation = fetched.conversation_id;
        if self.conversation(conversation).is_none() {
            // The server let the person read it, so it is a public channel they have not joined.
            let notice = "That message is in a channel you haven't joined. Join it to read it.";
            self.set_notice(Some(notice.to_owned()), out);
            self.join_offer = Some((conversation, link));
            return;
        }
        if let Some(root) = fetched.thread_root {
            return self.start_thread_job(link, conversation, root, fetched.seq, out);
        }
        self.select_conversation(conversation, out);
        self.permalink = Some(Job {
            link,
            stage: Stage::Paging {
                conversation,
                seq: fetched.seq,
                pages: 0,
            },
        });
        self.permalink_advance(out);
    }

    /// Opens the thread of a reply and starts looking for the reply among its replies.
    fn start_thread_job(
        &mut self,
        link: Permalink,
        conversation: ConversationId,
        root: MessageId,
        seq: EventSeq,
        out: &mut Output,
    ) {
        self.open_thread_input(conversation, root, out);
        if self.pane.as_ref().is_none_or(|pane| pane.root != root) {
            return;
        }
        self.permalink = Some(Job {
            link,
            stage: Stage::PagingThread {
                conversation,
                root,
                seq,
                pages: 0,
            },
        });
        self.permalink_advance(out);
    }

    /// Moves a job that is paging through a thread's replies forward.
    fn thread_job_advance(&mut self, out: &mut Output) {
        let Some(Job {
            link,
            stage:
                Stage::PagingThread {
                    conversation,
                    root,
                    seq,
                    pages,
                },
        }) = &self.permalink
        else {
            return;
        };
        let (link, conversation, root, seq, pages) = (*link, *conversation, *root, *seq, *pages);
        let Some(pane) = self.pane.as_ref().filter(|pane| pane.root == root) else {
            self.permalink = None;
            return;
        };
        if let Some(row) = pane.replies.message_row(link.message_id) {
            let key = row.key;
            self.permalink = None;
            return self.set_jump(
                Some(JumpTarget {
                    conversation,
                    key,
                    thread: Some(root),
                }),
                out,
            );
        }
        match &pane.replies.history {
            HistoryStatus::Loading => {}
            HistoryStatus::NotLoaded => {}
            HistoryStatus::Failed { message } => {
                let notice = format!("Couldn't load the replies to find that one. {message}");
                self.permalink = None;
                self.set_notice(Some(notice), out);
            }
            HistoryStatus::Ready { has_more } => {
                let reached = pane.replies.history_floor.is_none_or(|floor| floor <= seq);
                if reached || !*has_more {
                    self.permalink = None;
                    let notice = "That message isn't available. It may have been deleted.";
                    return self.set_notice(Some(notice.to_owned()), out);
                }
                if pages >= PERMALINK_MAX_PAGES {
                    self.permalink = None;
                    let notice = "That message is too far back to jump to. Scroll up to find it.";
                    return self.set_notice(Some(notice.to_owned()), out);
                }
                if let Some(Job {
                    stage: Stage::PagingThread { pages, .. },
                    ..
                }) = &mut self.permalink
                {
                    *pages += 1;
                }
                self.load_older_replies(root, out);
            }
        }
    }

    /// Moves a job that is paging forward: after the selection, and after each page arrives.
    pub(super) fn permalink_advance(&mut self, out: &mut Output) {
        self.thread_job_advance(out);
        let Some(Job {
            link,
            stage:
                Stage::Paging {
                    conversation,
                    seq,
                    pages,
                },
        }) = &self.permalink
        else {
            return;
        };
        let (link, conversation, seq, pages) = (*link, *conversation, *seq, *pages);
        if let Some(key) = self.loaded_key(conversation, link.message_id) {
            self.permalink = None;
            return self.set_jump(
                Some(JumpTarget {
                    conversation,
                    key,
                    thread: None,
                }),
                out,
            );
        }
        let Some(timeline) = self.timelines.get(&conversation) else {
            self.permalink = None;
            return;
        };
        match &timeline.history {
            HistoryStatus::Loading => {}
            HistoryStatus::NotLoaded => self.ensure_history(conversation, out),
            HistoryStatus::Failed { message } => {
                let notice = format!("Couldn't load the messages to find that one. {message}");
                self.permalink = None;
                self.set_notice(Some(notice), out);
            }
            HistoryStatus::Ready { has_more } => {
                let reached = timeline.history_floor.is_none_or(|floor| floor <= seq);
                if reached || !*has_more {
                    // The range that should hold the message is loaded and it is not there.
                    self.permalink = None;
                    let notice = "That message isn't available. It may have been deleted.";
                    return self.set_notice(Some(notice.to_owned()), out);
                }
                if pages >= PERMALINK_MAX_PAGES {
                    self.permalink = None;
                    let notice = "That message is too far back to jump to. Scroll up to find it.";
                    return self.set_notice(Some(notice.to_owned()), out);
                }
                let before = timeline.older_cursor();
                if let Some(Job {
                    stage: Stage::Paging { pages, .. },
                    ..
                }) = &mut self.permalink
                {
                    *pages += 1;
                }
                self.request_history(conversation, before, out);
            }
        }
    }

    /// The message a job (or the jump target) points at was deleted.
    pub(super) fn permalink_message_deleted(&mut self, message: MessageId, out: &mut Output) {
        if self
            .permalink
            .as_ref()
            .is_some_and(|job| job.link.message_id == message)
        {
            self.permalink = None;
            let notice = "That message was deleted.";
            self.set_notice(Some(notice.to_owned()), out);
        }
        if let Some(target) = self.jump
            && !self.jump_target_present(target)
        {
            self.set_jump(None, out);
        }
    }

    /// Whether the row a jump target names is still shown where it was.
    fn jump_target_present(&self, target: JumpTarget) -> bool {
        match target.thread {
            Some(root) => self.pane.as_ref().is_some_and(|pane| {
                pane.root == root && pane.replies.index_of(target.key).is_some()
            }),
            None => self
                .timelines
                .get(&target.conversation)
                .is_some_and(|timeline| timeline.index_of(target.key).is_some()),
        }
    }

    /// Runs after every input: drops state that no longer applies.
    ///
    /// * A job that is paging stops when the person goes to another conversation.
    /// * The jump target is forgotten when they leave its conversation or its row disappears.
    /// * A join offer is taken up once the channel is joined: the link opens.
    pub(super) fn permalink_housekeeping(&mut self, out: &mut Output) {
        if let Some(Job {
            stage: Stage::Paging { conversation, .. },
            ..
        }) = &self.permalink
            && self.selected != Some(*conversation)
        {
            self.permalink = None;
        }
        if let Some(Job {
            stage: Stage::PagingThread { root, .. },
            ..
        }) = &self.permalink
            && self.pane.as_ref().is_none_or(|pane| pane.root != *root)
        {
            self.permalink = None;
        }
        if let Some(target) = self.jump
            && (self.selected != Some(target.conversation) || !self.jump_target_present(target))
        {
            self.set_jump(None, out);
        }
        if let Some((conversation, link)) = self.join_offer
            && self.conversation(conversation).is_some()
        {
            self.join_offer = None;
            self.set_notice(None, out);
            self.open_permalink(link, out);
        }
    }

    /// Typing a new selection abandons a link that was still being fetched.
    pub(super) fn permalink_selection_chosen(&mut self) {
        if self
            .permalink
            .as_ref()
            .is_some_and(|job| matches!(job.stage, Stage::Fetching))
        {
            self.permalink = None;
        }
    }
}
