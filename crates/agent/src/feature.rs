// SPDX-License-Identifier: GPL-3.0-or-later

//! Features with their own D-Bus interfaces, apart from the desktop thread.

use zbus::connection::Builder;

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
}
