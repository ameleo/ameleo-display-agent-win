// SPDX-License-Identifier: GPL-3.0-or-later

//! Calls from the agent's threads to the host.
//!
//! The D-Bus session with the host lives on the session task, and comes and
//! goes with the host. A [`Host`] runs each call there with the connection
//! of the moment, and brings the answer back on a channel, so that Windows
//! threads, such as the clipboard rendering, can wait for it. Signals go
//! the same way.

use std::future::Future;
use std::pin::Pin;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use tokio::sync::mpsc;
use zbus::Connection;

use crate::desktop::Notify;

/// Longest wait for one call to the host. The host may forward it to its
/// client. Callers waiting on the reply may give up sooner.
pub const HOST_CALL_TIMEOUT: Duration = Duration::from_secs(55);

/// The answer to a call: a MIME type and data for a clipboard request.
pub type HostReply = Result<(String, Vec<u8>), String>;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
pub type CallFn = Box<dyn FnOnce(Connection) -> BoxFuture<HostReply> + Send>;
pub type EmitFn = Box<dyn FnOnce(Connection) -> BoxFuture<()> + Send>;

/// The way to the host, from any thread.
#[derive(Clone)]
pub struct Host {
    tx: mpsc::UnboundedSender<Notify>,
}

impl Host {
    pub fn new(tx: mpsc::UnboundedSender<Notify>) -> Self {
        Self { tx }
    }

    /// Starts a call to the host: `call` runs on the session task with the
    /// connection of the moment, within [`HOST_CALL_TIMEOUT`]. `what`
    /// names the call in the logs. Never blocks; the answer comes on the
    /// returned channel, an error when there is no session.
    pub fn start<F, Fut>(
        &self,
        what: impl Into<String>,
        call: F,
    ) -> Result<std_mpsc::Receiver<HostReply>, String>
    where
        F: FnOnce(Connection) -> Fut + Send + 'static,
        Fut: Future<Output = HostReply> + Send + 'static,
    {
        let (reply, rx) = std_mpsc::sync_channel(1);
        self.tx
            .send(Notify::Call {
                what: what.into(),
                call: Box::new(move |conn| Box::pin(call(conn))),
                reply,
            })
            .map_err(|_| "session gone".to_string())?;
        Ok(rx)
    }

    /// Runs `emit` on the session task with the connection of the moment,
    /// to send a signal; dropped when there is no session. Never blocks.
    pub fn emit<F, Fut>(&self, emit: F)
    where
        F: FnOnce(Connection) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let _ = self
            .tx
            .send(Notify::Emit(Box::new(move |conn| Box::pin(emit(conn)))));
    }
}
