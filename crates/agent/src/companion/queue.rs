// SPDX-License-Identifier: GPL-3.0-only

//! The agent's calls to the companion (`fr.ameleo.Companion1`), in order.
//!
//! The events (`HostConnected`, `SessionLocked`, `Reset`, the clipboard's)
//! go to the companion one at a time, in the order the agent made them: a
//! single task makes each call and waits for its answer, or for
//! [`EVENT_TIMEOUT`], before the next one.
//!
//! The host's calls relayed to the companion, and `ReadClipboard`, wait at
//! the [`Events::barrier`] until the events queued before them are
//! handled. With `Agent1` handled one call at a time (`spawn = false`), a
//! `Reset` is queued before zbus reads the host's next message: the
//! companion has handled it before any call of the next client session
//! reaches it, however late the reset came.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use zbus::Connection;

use super::relay::Target;
use super::{COMPANION_IFACE, COMPANION_PATH};

/// Longest wait for the companion to handle an event.
pub const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

type Call = Pin<Box<dyn Future<Output = zbus::Result<zbus::Message>> + Send>>;
type Job = Box<dyn FnOnce(Connection) -> Call + Send>;

/// The ordered queue of events to the companion.
pub struct Events {
    jobs: mpsc::UnboundedSender<(&'static str, Job)>,
    /// Events queued so far.
    queued: AtomicU64,
    /// Events handled (answered, failed or timed out) so far.
    done: watch::Receiver<u64>,
}

impl Events {
    /// Starts the task that makes the calls, on the current runtime, to the
    /// companion of the moment in `link`.
    pub fn new(link: Target) -> Arc<Self> {
        let (jobs, mut rx) = mpsc::unbounded_channel::<(&'static str, Job)>();
        let (done_tx, done) = watch::channel(0u64);
        tokio::spawn(async move {
            while let Some((member, job)) = rx.recv().await {
                // The companion of the moment; none, and the event is moot.
                let conn = link.lock().unwrap().clone();
                if let Some(conn) = conn {
                    match tokio::time::timeout(EVENT_TIMEOUT, job(conn)).await {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => tracing::warn!("companion {member} failed: {e}"),
                        Err(_) => {
                            tracing::warn!("companion {member}: no answer within {EVENT_TIMEOUT:?}")
                        }
                    }
                }
                done_tx.send_modify(|n| *n += 1);
            }
        });
        Arc::new(Self {
            jobs,
            queued: AtomicU64::new(0),
            done,
        })
    }

    /// Queues the call of `member` with `body`. Never blocks.
    pub fn post<B>(&self, member: &'static str, body: B)
    where
        B: serde::Serialize + zbus::zvariant::DynamicType + Send + Sync + 'static,
    {
        let job: Job = Box::new(move |conn: Connection| {
            Box::pin(async move {
                conn.call_method(
                    None::<&str>,
                    COMPANION_PATH,
                    Some(COMPANION_IFACE),
                    member,
                    &body,
                )
                .await
            })
        });
        // Counted before it is sent: a barrier taken after this call waits
        // for it.
        self.queued.fetch_add(1, Ordering::AcqRel);
        if self.jobs.send((member, job)).is_err() {
            // The task is gone (the runtime stops): nothing to wait for.
            self.queued.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Returns once the events queued before this call are handled.
    pub async fn barrier(&self) {
        let target = self.queued.load(Ordering::Acquire);
        let mut done = self.done.clone();
        let _ = done.wait_for(|n| *n >= target).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[tokio::test]
    async fn barrier_without_a_companion_returns() {
        let events = Events::new(Target::default());
        events.post("Reset", ());
        events.post("SessionLocked", (true,));
        tokio::time::timeout(Duration::from_secs(1), events.barrier())
            .await
            .expect("events without a companion are dropped at once");
    }

    #[tokio::test]
    async fn barrier_with_nothing_queued_returns() {
        let events = Events::new(Arc::new(Mutex::new(None)));
        tokio::time::timeout(Duration::from_secs(1), events.barrier())
            .await
            .unwrap();
    }
}
