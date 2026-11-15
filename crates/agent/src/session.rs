// SPDX-License-Identifier: GPL-3.0-or-later

//! Session manager: one zbus peer-to-peer server per host handshake.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use proto::AGENT_PATH;
use tokio::sync::mpsc;
use zbus::{AuthMechanism, Connection};

use crate::channel::port::{Port, PortEvent};
use crate::channel::transport;
use crate::desktop::{self, Notify};
use crate::ifaces::{AgentIface, Shared};

struct Session {
    generation: u64,
    data: mpsc::UnboundedSender<Vec<u8>>,
    conn: Option<Connection>,
}

pub async fn run() -> Result<()> {
    let (notify_tx, mut notify_rx) = mpsc::unbounded_channel();
    let shared = Shared::new(desktop::spawn(notify_tx)?);

    let (port, mut events, out) = loop {
        match Port::open(proto::PORT_NAME) {
            Ok(p) => break p,
            Err(e) => {
                tracing::warn!("cannot open port {}: {e}, retrying", proto::PORT_NAME);
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    };
    tracing::info!("port {} opened", proto::PORT_NAME);

    let (conn_tx, mut conn_rx) = mpsc::unbounded_channel::<(u64, zbus::Result<Connection>)>();
    let mut session: Option<Session> = None;
    let mut generation = 0u64;

    loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Some(PortEvent::NewSession) => {
                    generation += 1;
                    port.generation.store(generation, Ordering::Release);
                    if session.is_some() {
                        tracing::info!("new D-Bus handshake {generation}, dropping the previous session");
                    } else {
                        tracing::info!("D-Bus handshake {generation} from the host");
                    }
                    shared.reset();
                    let (split, data) = transport::session(generation, out.clone());
                    session = Some(Session { generation, data, conn: None });
                    let shared = shared.clone();
                    let conn_tx = conn_tx.clone();
                    let g = generation;
                    tokio::spawn(async move {
                        let _ = conn_tx.send((g, build(split, shared).await));
                    });
                }
                Some(PortEvent::Data(bytes)) => {
                    if let Some(s) = &session {
                        let _ = s.data.send(bytes);
                    }
                }
                Some(PortEvent::HostDisconnected) => {
                    tracing::info!("host disconnected, session {generation} closed");
                    generation += 1;
                    port.generation.store(generation, Ordering::Release);
                    if session.take().is_some() {
                        shared.reset();
                    }
                }
                None => anyhow::bail!("port reader stopped"),
            },
            Some((g, res)) = conn_rx.recv() => {
                match (&mut session, res) {
                    (Some(s), Ok(conn)) if s.generation == g => {
                        tracing::info!("D-Bus session {g} established");
                        s.conn = Some(conn);
                    }
                    (Some(s), Err(e)) if s.generation == g => {
                        tracing::warn!("D-Bus handshake {g} failed: {e}");
                        session = None;
                    }
                    _ => {} // stale
                }
            },
            Some(n) = notify_rx.recv() => {
                let conn = session.as_ref().and_then(|s| s.conn.clone());
                handle_notify(n, conn, &shared);
            },
        }
    }
}

async fn build(split: transport::Split, shared: Arc<Shared>) -> zbus::Result<Connection> {
    zbus::connection::Builder::socket(split)
        .server(zbus::Guid::generate())?
        .p2p()
        .auth_mechanism(AuthMechanism::Anonymous)
        .serve_at(AGENT_PATH, AgentIface(shared))?
        .build()
        .await
}

fn handle_notify(n: Notify, conn: Option<Connection>, shared: &Arc<Shared>) {
    match n {
        Notify::SessionLocked(locked) => {
            tracing::info!(locked, "session lock state");
            shared.locked.store(locked, Ordering::Relaxed);
            if let Some(conn) = conn {
                tokio::spawn(async move {
                    if let Ok(iface) = conn
                        .object_server()
                        .interface::<_, AgentIface>(AGENT_PATH)
                        .await
                    {
                        let _ = iface
                            .get()
                            .await
                            .session_locked_changed(iface.signal_emitter())
                            .await;
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::port::{FrameOut, Framer};

    /// Host client <-> guest server through the framer and the transport,
    /// as on the real port.
    #[tokio::test]
    async fn p2p_session_over_framed_stream() {
        let (notify_tx, _notify_rx) = mpsc::unbounded_channel();
        let shared = Shared::new(desktop::spawn(notify_tx).unwrap());

        let (g_out_tx, mut g_out_rx) = mpsc::channel(64);
        let (c_out_tx, mut c_out_rx) = mpsc::channel(64);
        let (gsplit, g_in) = transport::session(1, g_out_tx);
        let (csplit, c_in) = transport::session(1, c_out_tx);

        // client -> framer -> guest
        tokio::spawn(async move {
            let mut framer = Framer::default();
            let mut sessions = 0;
            while let Some((_, bytes)) = c_out_rx.recv().await {
                framer.feed(&bytes, |o| match o {
                    FrameOut::NewSession => sessions += 1,
                    FrameOut::Bytes(b) => {
                        let _ = g_in.send(b.to_vec());
                    }
                });
                assert_eq!(sessions, 1);
            }
        });
        // guest -> client
        tokio::spawn(async move {
            while let Some((_, bytes)) = g_out_rx.recv().await {
                let _ = c_in.send(bytes);
            }
        });

        let server = tokio::spawn(build(gsplit, shared));
        let client = zbus::connection::Builder::socket(csplit)
            .p2p()
            .auth_mechanism(AuthMechanism::Anonymous)
            .build()
            .await
            .unwrap();
        let _server = server.await.unwrap().unwrap();

        let agent = proto::AgentProxy::builder(&client)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .unwrap();
        assert_eq!(agent.version().await.unwrap(), env!("CARGO_PKG_VERSION"));
        assert!(agent.channels().await.unwrap().is_empty());
    }
}
