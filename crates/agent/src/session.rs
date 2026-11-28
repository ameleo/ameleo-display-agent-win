// SPDX-License-Identifier: GPL-3.0-only

//! Session manager: one zbus peer-to-peer server per host handshake.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use proto::AGENT_PATH;
use tokio::sync::mpsc;
use zbus::Connection;
use zbus::connection::AuthMechanism;

use crate::camera::Cameras;
use crate::channel::port::{Port, PortEvent};
use crate::channel::transport;
use crate::consent::Consent;
use crate::desktop::Cmd;
use crate::desktop::{self, Notify};
use crate::drive::ClientDrives;
use crate::feature::Feature;
use crate::file_clipboard::FileClipboard;
use crate::filexfer::FileTransfer;
use crate::host::{HOST_CALL_TIMEOUT, Host};
use crate::ifaces::{AgentIface, ClipboardIface, DisplayIface, MouseIface, Shared};

struct Session {
    generation: u64,
    data: mpsc::UnboundedSender<Vec<u8>>,
    conn: Option<Connection>,
}

pub async fn run() -> Result<()> {
    let (notify_tx, mut notify_rx) = mpsc::unbounded_channel();
    let locked = Arc::new(AtomicBool::new(false));
    let mut features: Vec<Arc<dyn Feature>> = vec![
        Arc::new(FileTransfer::new(locked.clone())),
        Arc::new(ClientDrives::new()),
        Arc::new(FileClipboard::new(
            Host::new(notify_tx.clone()),
            locked.clone(),
        )),
    ];
    match Consent::new(locked.clone()) {
        Ok(c) => features.push(Arc::new(c)),
        Err(e) => tracing::warn!("nobody may view or control the session: {e}"),
    }
    match Cameras::new(Host::new(notify_tx.clone())) {
        Ok(c) => features.push(Arc::new(c)),
        Err(e) => tracing::warn!("no client cameras: {e:#}"),
    }
    let formats = features
        .iter()
        .filter_map(|f| f.clipboard_formats())
        .collect();
    let shared = Shared::new(desktop::spawn(notify_tx, formats)?, locked, features);
    // The host empties the guest of what its clients added at client
    // disconnection (Agent1.Reset) or session end; this covers stopping
    // without one (service stop, VM shutdown with a client connected).
    {
        let shared = shared.clone();
        crate::service::on_stop(move || {
            for f in &shared.features {
                f.stop();
            }
        });
    }

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
                    shared.connected(None);
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
                    shared.connected(None);
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
                        shared.connected(Some(&conn));
                        s.conn = Some(conn);
                        // The re-announce done by the reset at handshake time
                        // had no connection to go to: send it now.
                        shared.desktop.send(Cmd::Announce);
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
    let mut builder = zbus::connection::Builder::socket(split)
        .server(zbus::Guid::generate())
        .p2p()
        .auth_mechanism(AuthMechanism::Anonymous)
        .serve_at(AGENT_PATH, AgentIface(shared.clone()))
        .serve_at(AGENT_PATH, MouseIface(shared.clone()))
        .serve_at(AGENT_PATH, ClipboardIface(shared.clone()))
        .serve_at(AGENT_PATH, DisplayIface(shared.clone()));
    for f in &shared.features {
        builder = f.serve(builder)?;
    }
    builder.build().await
}

fn handle_notify(n: Notify, conn: Option<Connection>, shared: &Arc<Shared>) {
    match n {
        Notify::ClipboardGrabbed { serial, mimes } => {
            let Some(conn) = conn else {
                tracing::info!(
                    serial,
                    "guest clipboard change not sent: no D-Bus session yet"
                );
                return;
            };
            tokio::spawn(async move {
                if let Ok(iface) = conn
                    .object_server()
                    .interface::<_, ClipboardIface>(AGENT_PATH)
                    .await
                {
                    tracing::info!(serial, ?mimes, "signal Grabbed sent to the host");
                    if let Err(e) =
                        ClipboardIface::grabbed(iface.signal_emitter(), serial, mimes).await
                    {
                        tracing::warn!("cannot send Grabbed: {e}");
                    }
                }
            });
        }
        Notify::ClipboardReleased => {
            let Some(conn) = conn else { return };
            tokio::spawn(async move {
                if let Ok(iface) = conn
                    .object_server()
                    .interface::<_, ClipboardIface>(AGENT_PATH)
                    .await
                {
                    tracing::info!("signal Released sent to the host");
                    let _ = ClipboardIface::released(iface.signal_emitter()).await;
                }
            });
        }
        Notify::Call { what, call, reply } => {
            let Some(conn) = conn else {
                tracing::warn!("host {what}: no D-Bus session");
                let _ = reply.send(Err("no D-Bus session".into()));
                return;
            };
            tokio::spawn(async move {
                let t = std::time::Instant::now();
                let res = tokio::time::timeout(HOST_CALL_TIMEOUT, call(conn))
                    .await
                    .unwrap_or_else(|_| Err(format!("no answer within {HOST_CALL_TIMEOUT:?}")));
                match &res {
                    Ok((_, d)) => {
                        tracing::debug!(took = ?t.elapsed(), bytes = d.len(), "host {what}")
                    }
                    Err(e) => tracing::warn!(took = ?t.elapsed(), "host {what} failed: {e}"),
                }
                let _ = reply.send(res);
            });
        }
        Notify::Emit(emit) => match conn {
            Some(conn) => {
                tokio::spawn(emit(conn));
            }
            None => tracing::debug!("signal dropped: no D-Bus session"),
        },
        Notify::SessionLocked(locked) => {
            tracing::info!(locked, "session lock state");
            shared.locked.store(locked, Ordering::Relaxed);
            for f in &shared.features {
                f.session_locked(locked);
            }
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
        let locked = Arc::new(AtomicBool::new(false));
        let mut features: Vec<Arc<dyn Feature>> = vec![Arc::new(FileTransfer::new(locked.clone()))];
        // Only where Windows can make virtual cameras (not Windows 10).
        let cameras = Cameras::new(Host::new(notify_tx.clone())).ok();
        if let Some(c) = cameras {
            features.push(Arc::new(c));
        }
        let has_cameras = features.len() == 2;
        let shared = Shared::new(
            desktop::spawn(notify_tx, Vec::new()).unwrap(),
            locked,
            features,
        );

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
        let mut channels = vec![
            proto::iface::MOUSE,
            proto::iface::CLIPBOARD,
            proto::iface::DISPLAY,
            proto::iface::FILE_TRANSFER,
        ];
        if has_cameras {
            channels.push(proto::iface::CAMERA);
        }
        assert_eq!(agent.channels().await.unwrap(), channels);

        let cb = proto::ClipboardProxy::builder(&client)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .unwrap();
        cb.set_max_size(4096).await.unwrap();
        assert_eq!(cb.max_size().await.unwrap(), 4096);

        let ft = proto::FileTransferProxy::builder(&client)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .unwrap();
        let err = ft.write(42, vec![0; 10]).await.unwrap_err();
        assert!(err.to_string().contains("NotFound"), "{err}");

        if !has_cameras {
            return;
        }
        // Formats are checked before any virtual camera is created.
        let cam = proto::CameraProxy::builder(&client)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .unwrap();
        let err = cam.add_camera(1, "none", vec![]).await.unwrap_err();
        assert!(err.to_string().contains("InvalidArgument"), "{err}");
        let err = cam
            .add_camera(1, "odd", vec![(641, 480, 30, 1)])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("InvalidArgument"), "{err}");
        let err = cam
            .add_camera(1, "8k", vec![(7680, 4320, 30, 1)])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("InvalidArgument"), "{err}");
    }
}
