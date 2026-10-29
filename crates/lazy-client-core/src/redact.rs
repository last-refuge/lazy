//! Printing that cannot leak (slice 3I, D24): the `Debug` text of `Input` and `Effect` (and of the
//! few other types that can carry typed text) shows what happened and to what (the variant, the
//! identifiers, the numbers, the lengths of text) and never the text, the names, the paths, or the
//! secrets themselves. A field whose type is not known to be harmless prints as `<TypeName>`, so
//! a new kind of content is hidden until someone decides it may be shown.

use std::fmt::{self, Debug};

use lazy_protocol::ServerFrame;

use crate::attachments::{FileSource, TransferError};
use crate::model::RequestError;

/// Text: only how long it is.
pub(crate) struct Text<'a>(pub &'a str);

impl Debug for Text<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<redacted {} chars>", self.0.chars().count())
    }
}

/// Optional text.
pub(crate) struct OptText<'a>(pub &'a Option<String>);

impl Debug for OptText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            None => f.write_str("None"),
            Some(text) => write!(f, "Some({:?})", Text(text)),
        }
    }
}

/// A value that is only named.
pub(crate) struct Named(pub &'static str);

impl Debug for Named {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{}>", self.0)
    }
}

/// An optional value that is only named when it is there.
pub(crate) struct OptNamed<'a, T>(pub &'a Option<T>, pub &'static str);

impl<T> Debug for OptNamed<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            None => f.write_str("None"),
            Some(_) => write!(f, "Some(<{}>)", self.1),
        }
    }
}

/// A kind of thing found at run time.
pub(crate) struct Kind(pub String);

impl Debug for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{}>", self.0)
    }
}

/// How many of something.
pub(crate) struct Count(pub &'static str, pub usize);

impl Debug for Count {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} {}>", self.1, self.0)
    }
}

/// A file the person chose: a path (not shown) or pasted bytes (their number).
pub(crate) struct Source<'a>(pub &'a FileSource);

impl Debug for Source<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            FileSource::Path(_) => f.write_str("<path>"),
            FileSource::Bytes(bytes) => write!(f, "<{} bytes>", bytes.len()),
        }
    }
}

/// A failed request: the server's code, or that the outcome is unknown (the detail is not shown:
/// it can quote what was sent or received).
pub(crate) struct Req<'a>(pub &'a RequestError);

impl Debug for Req<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            RequestError::Rejected(api) => write!(f, "Rejected({:?})", api.code),
            RequestError::Unknown(detail) => {
                write!(f, "Unknown(<{} chars>)", detail.chars().count())
            }
        }
    }
}

/// A failed transfer: which way it failed, never the reason (which can hold a path).
pub(crate) struct Xfer<'a>(pub &'a TransferError);

impl Debug for Xfer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            TransferError::Request(error) => write!(f, "Request({:?})", Req(error)),
            TransferError::TooLarge { size } => write!(f, "TooLarge {{ size: {size} }}"),
            TransferError::Unreadable(_) => f.write_str("Unreadable"),
            TransferError::Missing => f.write_str("Missing"),
            TransferError::Disk(_) => f.write_str("Disk"),
            TransferError::ChecksumMismatch => f.write_str("ChecksumMismatch"),
            TransferError::Cancelled => f.write_str("Cancelled"),
        }
    }
}

/// The answer to a request: what kind of thing came, or why not.
pub(crate) struct ResReq<'a, T>(pub &'a Result<T, RequestError>, pub &'static str);

impl<T> Debug for ResReq<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Ok(_) => write!(f, "Ok(<{}>)", self.1),
            Err(error) => write!(f, "Err({:?})", Req(error)),
        }
    }
}

/// A result whose success is only named and whose failure is a phrase (shown as its length).
pub(crate) struct ResText<'a, T>(pub &'a Result<T, String>, pub &'static str);

impl<T> Debug for ResText<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Ok(_) => write!(f, "Ok(<{}>)", self.1),
            Err(why) => write!(f, "Err(<{} chars>)", why.chars().count()),
        }
    }
}

pub(crate) struct ResXfer<'a, T>(pub &'a Result<T, TransferError>, pub &'static str);

impl<T> Debug for ResXfer<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Ok(_) => write!(f, "Ok(<{}>)", self.1),
            Err(error) => write!(f, "Err({:?})", Xfer(error)),
        }
    }
}

/// The name of an enum variant, from the start of its `Debug` text.
pub(crate) fn variant_name(debug: &str) -> &str {
    let end = debug
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(debug.len());
    &debug[..end]
}

/// A frame from the server: where it is from and what kind it is, not what it says.
pub(crate) struct Frame<'a>(pub &'a ServerFrame);

impl Debug for Frame<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            ServerFrame::Event(envelope) => {
                let event = format!("{:?}", envelope.event);
                f.debug_struct("Event")
                    .field("workspace", &envelope.workspace_id)
                    .field("seq", &envelope.seq)
                    .field("kind", &Kind(variant_name(&event).to_owned()))
                    .finish()
            }
            ServerFrame::CaughtUp { workspace_id, seq } => f
                .debug_struct("CaughtUp")
                .field("workspace", workspace_id)
                .field("seq", seq)
                .finish(),
            ServerFrame::SnapshotRequired {
                workspace_id,
                reason,
            } => f
                .debug_struct("SnapshotRequired")
                .field("workspace", workspace_id)
                .field("reason", reason)
                .finish(),
            ServerFrame::Error { code, message } => f
                .debug_struct("Error")
                .field("code", code)
                .field("message", &Text(message))
                .finish(),
        }
    }
}

// ----- Types that can carry what the person typed -----

impl Debug for crate::model::JournalEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JournalEntry")
            .field("operation_id", &self.operation_id)
            .field("conversation_id", &self.conversation_id)
            .field("kind", &self.kind)
            .field("document", &Named("document"))
            .field("thread_root", &self.thread_root)
            .field("also_in_channel", &self.also_in_channel)
            .field("attachments", &Count("files", self.attachments.len()))
            .finish()
    }
}

impl Debug for crate::cache::ContentCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContentCache")
            .field("workspaces", &Count("workspaces", self.workspaces.len()))
            .field("messages", &self.message_count())
            .finish()
    }
}

impl Debug for crate::model::StartupState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StartupState")
            .field("session_restored", &self.session_restored)
            .field("journal", &Count("entries", self.journal.len()))
            .field("drafts", &Count("drafts", self.drafts.len()))
            .field("thread_drafts", &Count("drafts", self.thread_drafts.len()))
            .field("view", &self.view)
            .field("cache", &OptNamed(&self.cache, "ContentCache"))
            .field(
                "recent_searches",
                &Count("texts", self.recent_searches.len()),
            )
            .field(
                "recent_conversations",
                &Count("conversations", self.recent_conversations.len()),
            )
            .field("appearance", &self.appearance)
            .field("active_workspace", &self.active_workspace)
            .field("problems", &Count("texts", self.problems.len()))
            .field("server", &Named("ServerInfo"))
            .finish()
    }
}
