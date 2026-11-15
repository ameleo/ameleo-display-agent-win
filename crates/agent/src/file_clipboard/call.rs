// SPDX-License-Identifier: GPL-3.0-only

//! The host calls of the virtual file data object, and their answers.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;

use crate::win::Event;

/// A call of the data object to the host.
#[derive(Debug)]
pub enum HostCall {
    /// `Host.Clipboard1.Request`.
    Request { serial: u32, mimes: Vec<String> },
    /// `Host.FileClipboard1.FileContents`.
    FileContents {
        lindex: i32,
        flags: u32,
        position: u64,
        requested: u32,
        clip_data_id: Option<u32>,
    },
    /// `Host.FileClipboard1.LockClipData`.
    Lock(u32),
    /// `Host.FileClipboard1.UnlockClipData`.
    Unlock(u32),
}

/// The host's answer: (MIME type, data), or what went wrong.
pub type HostReply = Result<(String, Vec<u8>), String>;

/// Where a [`HostCall`] is answered. Also sets the event of the thread that
/// made the call, if it waits on one (the OLE thread, see
/// [`wake_on_replies`]), so that thread sleeps until the answer instead of
/// polling.
pub struct ReplyTx {
    tx: std_mpsc::SyncSender<HostReply>,
    wake: Option<Arc<Event>>,
}

impl ReplyTx {
    pub fn send(&self, reply: HostReply) -> Result<(), std_mpsc::SendError<HostReply>> {
        let res = self.tx.send(reply);
        // After the send: the woken thread finds the answer.
        if let Some(w) = &self.wake {
            w.set();
        }
        res
    }
}

impl Drop for ReplyTx {
    /// Dropped unanswered: the waiting thread sees the channel closed now,
    /// not at its timeout.
    fn drop(&mut self) {
        if let Some(w) = &self.wake {
            w.set();
        }
    }
}

thread_local! {
    static REPLY_WAKE: RefCell<Option<Arc<Event>>> = const { RefCell::new(None) };
}

/// Answers to the calls this thread makes also set `wake`.
pub fn wake_on_replies(wake: Arc<Event>) {
    REPLY_WAKE.with_borrow_mut(|w| *w = Some(wake));
}

/// A channel for one host answer.
pub fn reply_channel() -> (ReplyTx, std_mpsc::Receiver<HostReply>) {
    let (tx, rx) = std_mpsc::sync_channel(1);
    (
        ReplyTx {
            tx,
            wake: REPLY_WAKE.with_borrow(Clone::clone),
        },
        rx,
    )
}
