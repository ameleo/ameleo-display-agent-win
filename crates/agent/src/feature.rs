// SPDX-License-Identifier: GPL-3.0-or-later

//! Features with their own D-Bus interfaces, apart from the desktop thread.

use std::sync::Arc;

use zbus::Connection;
use zbus::connection::Builder;

/// Data made from the guest clipboard, off the desktop thread (see
/// [`ClipboardFormats::read`]).
pub type MakeData = Box<dyn FnOnce() -> proto::Result<Vec<u8>> + Send>;

/// A feature with its own D-Bus interfaces.
pub trait Feature: Send + Sync {
    /// The D-Bus interfaces it serves, announced in `Agent1.Channels`.
    fn channels(&self) -> Vec<&'static str>;

    /// Adds its interfaces to the D-Bus server of a new session, at
    /// [`proto::AGENT_PATH`].
    fn serve<'a>(&self, builder: Builder<'a>) -> zbus::Result<Builder<'a>>;

    /// A new D-Bus session, or `Agent1.Reset`: forget the client's state
    /// and remove what it added to the guest. Runs on an async thread: must
    /// not block.
    fn reset(&self);

    /// The agent stops: remove what the clients added to the guest. May
    /// block for a short while.
    fn stop(&self) {}

    /// The D-Bus session with the host: the connection once it is up,
    /// `None` when it goes away.
    fn connected(&self, _conn: Option<&Connection>) {}

    /// The session was locked or unlocked.
    fn session_locked(&self, _locked: bool) {}

    /// More clipboard formats, exchanged through `Clipboard1`. Asked once,
    /// when the agent starts.
    fn clipboard_formats(&self) -> Option<Arc<dyn ClipboardFormats>> {
        None
    }
}

/// Clipboard formats a feature adds to `Clipboard1`, such as a file list
/// offered through an OLE data object of its own. The agent calls these
/// methods on its desktop thread: they must answer quickly.
pub trait ClipboardFormats: Send + Sync {
    /// The MIME types it can export from the guest clipboard, announced
    /// before the agent's own. The clipboard is not open:
    /// `IsClipboardFormatAvailable` tells what it holds.
    fn guest_mimes(&self) -> Vec<String>;

    /// The guest clipboard changed and is announced to the host under
    /// `serial`.
    fn guest_changed(&self, serial: u32);

    /// The host reads the guest clipboard as `mime`, the clipboard being
    /// open: `None` when `mime` is not one of its types. The data is made
    /// by the returned function, on a blocking thread.
    fn read(&self, mime: &str, serial: u32) -> Option<Result<MakeData, String>>;

    /// The host owns the clipboard with `mimes`: `true` when it puts the
    /// host's data on the clipboard itself, instead of the agent.
    fn host_grab(&self, serial: u32, mimes: &[String]) -> bool;

    /// The host gave up the clipboard that it took, or another host grab
    /// replaced it.
    fn host_release(&self);

    /// The clipboard owner window of process `pid` is its own, such as a
    /// helper process: its changes are not copies made by the user.
    fn owns(&self, pid: u32) -> bool;
}
