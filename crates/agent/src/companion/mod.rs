// SPDX-License-Identifier: GPL-3.0-only

//! The companion service: another Windows service that implements more
//! `fr.ameleo.Agent.*` interfaces in its own process. The contract is
//! `proto::companion`.
//!
//! The agent keeps the port to the host. The companion connects to the
//! agent through the local named pipe [`PIPE_NAME`], open to LocalSystem
//! only, and speaks peer-to-peer D-Bus on it, the agent being the server:
//!
//! * The agent serves `fr.ameleo.CompanionLink1` at [`LINK_PATH`]. The
//!   companion calls `Register(agent, host)` with the `fr.ameleo.Agent.*`
//!   interfaces it serves at `/fr/ameleo/Agent` on the link, and the
//!   `fr.ameleo.Host.*` interfaces it calls at `/fr/ameleo/Host`.
//! * The agent announces the companion's interfaces in `Agent1.Channels`
//!   and relays the host's calls to them, and the companion's calls to the
//!   host, as they are (see [`relay`]).
//! * The companion serves `fr.ameleo.Companion1` at [`COMPANION_PATH`]: the
//!   agent calls `HostConnected(b)`, `SessionLocked(b)`, `Reset()` and,
//!   before it exits, `Stop()`, in order (see [`queue`]): a reset reaches
//!   the companion before the next client session's calls.
//! * The companion can also take part in `Clipboard1` (see [`clipboard`]).
//!
//! The agent knows nothing else of the companion. One companion at a time:
//! a new connection replaces the previous one.

mod clipboard;
mod link;
mod queue;
mod relay;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use proto::companion::{COMPANION_PATH, LINK_PATH, MAX_RELAYED_INTERFACES, PIPE_NAME, PIPE_SDDL};
use proto::{AGENT_PATH, HOST_PATH};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::oneshot;
use zbus::Connection;
use zbus::connection::AuthMechanism;
use zbus::connection::Builder;

use crate::feature::{ClipboardFormats, Feature};
use crate::ifaces::AgentIface;
use crate::win::wide;
use queue::{EVENT_TIMEOUT, Events};
use relay::Target;

const COMPANION_IFACE: &str = proto::companion::iface::COMPANION;

/// The interfaces the companion registered.
#[derive(Default)]
struct Registered {
    agent: Vec<String>,
    host: Vec<String>,
}

/// The companion, as a feature of the agent.
pub struct Companion {
    /// The link with the companion, while it is connected.
    to_companion: Target,
    /// Counts the companion connections.
    generation: AtomicU64,
    /// The D-Bus session with the host, while it is up.
    to_host: Target,
    registered: Mutex<Registered>,
    locked: Mutex<bool>,
    /// The interfaces the agent serves itself: never relayed.
    own: Vec<&'static str>,
    /// The agent's calls to the companion, in order.
    events: Arc<Events>,
    clipboard: Arc<clipboard::CompanionClipboard>,
    rt: tokio::runtime::Handle,
}

impl Companion {
    /// Starts listening for the companion. `own`: the interfaces the agent
    /// serves itself.
    pub fn start(own: Vec<&'static str>) -> std::io::Result<Arc<Self>> {
        let first = pipe(true)?;
        let me = Self::new(own);
        tokio::spawn(accept(me.clone(), first));
        Ok(me)
    }

    fn new(own: Vec<&'static str>) -> Arc<Self> {
        let to_companion = Target::default();
        let rt = tokio::runtime::Handle::current();
        let events = Events::new(to_companion.clone());
        Arc::new(Self {
            clipboard: clipboard::CompanionClipboard::new(
                to_companion.clone(),
                events.clone(),
                rt.clone(),
            ),
            events,
            to_companion,
            generation: AtomicU64::new(0),
            to_host: Target::default(),
            registered: Mutex::default(),
            locked: Mutex::new(false),
            own,
            rt,
        })
    }

    fn host(&self) -> Option<Connection> {
        self.to_host.lock().unwrap().clone()
    }

    /// A companion connected: it replaces the previous one. Returns its
    /// generation.
    async fn attach(self: &Arc<Self>, link: Connection) -> u64 {
        self.detach().await;
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        *self.to_companion.lock().unwrap() = Some(link);
        let locked = *self.locked.lock().unwrap();
        let host = self.host().is_some();
        self.events.post("SessionLocked", (locked,));
        self.events.post("HostConnected", (host,));
        self.events.barrier().await;
        generation
    }

    /// The companion went away: its interfaces too.
    async fn detach(&self) {
        *self.to_companion.lock().unwrap() = None;
        self.clipboard.clear();
        let old = std::mem::take(&mut *self.registered.lock().unwrap());
        if old.agent.is_empty() {
            return;
        }
        tracing::info!(interfaces = ?old.agent, "companion interfaces removed");
        if let Some(host) = self.host() {
            for name in &old.agent {
                let _ = relay::remove(&host, AGENT_PATH, name).await;
            }
            channels_changed(&host).await;
        }
    }

    /// `Register`: relays `agent` from the host and `host` to the host.
    async fn register(
        &self,
        link: &Connection,
        agent: Vec<String>,
        host: Vec<String>,
    ) -> proto::Result<()> {
        let bad =
            |what: &str, name: &str| Err(proto::Error::InvalidArgument(format!("{what}: {name}")));
        for name in &agent {
            if !name.starts_with("fr.ameleo.Agent.") || name == proto::iface::AGENT {
                return bad("not an agent interface", name);
            }
            if self.own.contains(&name.as_str()) {
                return bad("served by the agent", name);
            }
        }
        if let Some(name) = host.iter().find(|n| !n.starts_with("fr.ameleo.Host.")) {
            return bad("not a host interface", name);
        }
        if agent.iter().chain(&host).any(|n| relay::slot(n).is_none()) {
            return Err(proto::Error::InvalidArgument(format!(
                "more than {} relayed interfaces",
                MAX_RELAYED_INTERFACES
            )));
        }
        let old = std::mem::replace(
            &mut *self.registered.lock().unwrap(),
            Registered {
                agent: agent.clone(),
                host: host.clone(),
            },
        );
        tracing::info!(?agent, ?host, "companion registered");
        let failed = |e: zbus::Error| proto::Error::Failed(e.to_string());
        for name in old.host.iter().filter(|n| !host.contains(n)) {
            relay::remove(link, HOST_PATH, name).await.map_err(failed)?;
        }
        for name in host.iter().filter(|n| !old.host.contains(n)) {
            relay::add(link, HOST_PATH, name, self.to_host.clone(), None)
                .await
                .map_err(failed)?;
        }
        if let Some(conn) = self.host() {
            for name in old.agent.iter().filter(|n| !agent.contains(n)) {
                relay::remove(&conn, AGENT_PATH, name)
                    .await
                    .map_err(failed)?;
            }
            for name in agent.iter().filter(|n| !old.agent.contains(n)) {
                relay::add(
                    &conn,
                    AGENT_PATH,
                    name,
                    self.to_companion.clone(),
                    Some(self.events.clone()),
                )
                .await
                .map_err(failed)?;
            }
            channels_changed(&conn).await;
        }
        Ok(())
    }
}

impl Feature for Companion {
    fn channels(&self) -> Vec<&'static str> {
        let registered = self.registered.lock().unwrap();
        registered
            .agent
            .iter()
            .filter_map(|n| relay::slot(n).and_then(relay::name))
            .collect()
    }

    fn serve<'a>(&self, mut builder: Builder<'a>) -> zbus::Result<Builder<'a>> {
        let agent = self.registered.lock().unwrap().agent.clone();
        for name in agent {
            builder = relay::serve(
                builder,
                AGENT_PATH,
                &name,
                self.to_companion.clone(),
                Some(self.events.clone()),
            )?;
        }
        Ok(builder)
    }

    fn connected(&self, conn: Option<&Connection>) {
        *self.to_host.lock().unwrap() = conn.cloned();
        self.events.post("HostConnected", (conn.is_some(),));
    }

    fn session_locked(&self, locked: bool) {
        *self.locked.lock().unwrap() = locked;
        self.events.post("SessionLocked", (locked,));
    }

    /// Queued: the host's next calls to the companion wait for it (see
    /// [`queue`]).
    fn reset(&self) {
        self.events.post("Reset", ());
    }

    fn clipboard_formats(&self) -> Option<Arc<dyn ClipboardFormats>> {
        Some(self.clipboard.clone())
    }

    fn stop(&self) {
        self.events.post("Stop", ());
        // Not on the runtime: the stop comes from a thread of its own. The
        // timeout is made inside `block_on`: its timer needs the runtime.
        let events = self.events.clone();
        let _ = self
            .rt
            .block_on(async { tokio::time::timeout(2 * EVENT_TIMEOUT, events.barrier()).await });
    }
}

/// `fr.ameleo.CompanionLink1`, served to the companion.
struct LinkIface(Arc<Companion>);

#[zbus::interface(name = "fr.ameleo.CompanionLink1")]
impl LinkIface {
    async fn register(
        &self,
        agent: Vec<String>,
        host: Vec<String>,
        #[zbus(connection)] link: &Connection,
    ) -> proto::Result<()> {
        self.0.register(link, agent, host).await
    }

    /// The clipboard formats the companion handles: (Windows clipboard
    /// format, MIME type) pairs. A predefined format is given as `#<id>`,
    /// such as `#15` for `CF_HDROP`.
    fn register_clipboard(&self, formats: Vec<(String, String)>) -> proto::Result<()> {
        self.0.clipboard.register(formats)
    }

    /// The processes whose clipboard changes are the companion's own, such
    /// as its helper in the user session.
    fn set_clipboard_owners(&self, pids: Vec<u32>) {
        self.0.clipboard.set_owners(pids);
    }
}

/// Tells the host that `Agent1.Channels` changed.
async fn channels_changed(conn: &Connection) {
    if let Ok(iface) = conn
        .object_server()
        .interface::<_, AgentIface>(AGENT_PATH)
        .await
    {
        let _ = iface
            .get()
            .await
            .channels_changed(iface.signal_emitter())
            .await;
    }
}

/// An instance of the companion's pipe. `first` fails if another process
/// already owns the name.
fn pipe(first: bool) -> std::io::Result<NamedPipeServer> {
    use windows_sys::Win32::Foundation::{HLOCAL, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide(PIPE_SDDL).as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    let res = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                PIPE_NAME,
                (&mut sa as *mut SECURITY_ATTRIBUTES).cast(),
            )
    };
    unsafe { LocalFree(sd as HLOCAL) };
    res
}

/// Waits for companions, one connection at a time.
async fn accept(me: Arc<Companion>, mut pipe: NamedPipeServer) {
    loop {
        if let Err(e) = pipe.connect().await {
            tracing::warn!("companion pipe: {e}");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let next = match self::pipe(false) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("cannot open the companion pipe again, no companion: {e}");
                return;
            }
        };
        let connected = std::mem::replace(&mut pipe, next);
        let me = me.clone();
        tokio::spawn(async move { serve_link(me, connected).await });
    }
}

/// One companion, until it goes away.
async fn serve_link<S>(me: Arc<Companion>, pipe: S)
where
    S: tokio::io::AsyncRead
        + tokio::io::AsyncWrite
        + Send
        + Sync
        + Unpin
        + std::fmt::Debug
        + 'static,
{
    let (closed_tx, closed) = oneshot::channel();
    let link = async {
        Builder::socket(link::split(pipe, closed_tx))
            .server(zbus::Guid::generate())
            .p2p()
            .auth_mechanism(AuthMechanism::Anonymous)
            .serve_at(LINK_PATH, LinkIface(me.clone()))
            .build()
            .await
    };
    let link = match link.await {
        Ok(link) => link,
        Err(e) => {
            tracing::warn!("companion handshake failed: {e}");
            return;
        }
    };
    tracing::info!("companion connected");
    let generation = me.attach(link).await;
    let _ = closed.await;
    // A newer companion may have replaced this one already.
    if me.generation.load(Ordering::Acquire) == generation {
        tracing::info!("companion gone");
        me.detach().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    /// The companion side of `fr.ameleo.Companion1`: counts the events.
    #[derive(Default)]
    struct Events {
        resets: Arc<AtomicU32>,
        host: Arc<Mutex<Option<bool>>>,
    }

    #[zbus::interface(name = "fr.ameleo.Companion1")]
    impl Events {
        fn host_connected(&self, connected: bool) {
            *self.host.lock().unwrap() = Some(connected);
        }
        fn session_locked(&self, _locked: bool) {}
        /// Slow: a call relayed right after must still find it done.
        async fn reset(&self) {
            tokio::time::sleep(Duration::from_millis(200)).await;
            self.resets.fetch_add(1, Ordering::AcqRel);
        }
        fn stop(&self) {}
    }

    /// A relayed interface; it sees the resets the companion handled.
    struct Drives(Arc<AtomicU32>);
    #[zbus::interface(name = "fr.ameleo.Agent.CompanionTest1")]
    impl Drives {
        fn add(&self, id: u32) -> String {
            format!("drive {id}")
        }
        fn resets_seen(&self) -> u32 {
            self.0.load(Ordering::Acquire)
        }
    }

    struct HostSide;
    #[zbus::interface(name = "fr.ameleo.Host.CompanionTest1")]
    impl HostSide {
        fn read(&self, n: u32) -> Vec<u8> {
            vec![1; n as usize]
        }
    }

    async fn channels(host: &Connection) -> Vec<String> {
        proto::AgentProxy::builder(host)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .unwrap()
            .channels()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn companion_interfaces_are_relayed() {
        let me = Companion::new(vec![proto::iface::MOUSE]);

        // The host session, with the agent's own interfaces.
        let (a, b) = tokio::io::duplex(1 << 16);
        struct Agent(Arc<Companion>);
        #[zbus::interface(name = "fr.ameleo.Agent1")]
        impl Agent {
            #[zbus(property)]
            fn channels(&self) -> Vec<String> {
                self.0.channels().into_iter().map(String::from).collect()
            }
        }
        let agent_side = me.serve(
            Builder::socket(link::split(a, oneshot::channel().0))
                .server(zbus::Guid::generate())
                .p2p()
                .auth_mechanism(AuthMechanism::Anonymous)
                .serve_at(AGENT_PATH, Agent(me.clone())),
        );
        let host_side = Builder::socket(link::split(b, oneshot::channel().0))
            .p2p()
            .auth_mechanism(AuthMechanism::Anonymous)
            .serve_at(HOST_PATH, HostSide);
        let (agent_h, host) = tokio::join!(agent_side.unwrap().build(), host_side.build());
        let (agent_h, host) = (agent_h.unwrap(), host.unwrap());
        me.connected(Some(&agent_h));
        assert!(channels(&host).await.is_empty());

        // The companion connects and registers.
        let (a, b) = tokio::io::duplex(1 << 16);
        let link_task = tokio::spawn(serve_link(me.clone(), a));
        let events = Events::default();
        let (resets, seen_host) = (events.resets.clone(), events.host.clone());
        let comp = Builder::socket(link::split(b, oneshot::channel().0))
            .p2p()
            .auth_mechanism(AuthMechanism::Anonymous)
            .serve_at(COMPANION_PATH, events)
            .serve_at(AGENT_PATH, Drives(resets.clone()))
            .build()
            .await
            .unwrap();
        let link = proto::companion::CompanionLinkProxy::builder(&comp)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .unwrap();
        let register = move |agent: Vec<&str>, host: Vec<&str>| {
            let link = link.clone();
            let agent: Vec<String> = agent.into_iter().map(String::from).collect();
            let host: Vec<String> = host.into_iter().map(String::from).collect();
            async move { link.register(agent, host).await }
        };
        // The agent's own interfaces stay its own.
        let err = register(vec![proto::iface::MOUSE], vec![])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("served by the agent"), "{err}");
        register(
            vec!["fr.ameleo.Agent.CompanionTest1"],
            vec!["fr.ameleo.Host.CompanionTest1"],
        )
        .await
        .unwrap();
        assert_eq!(channels(&host).await, ["fr.ameleo.Agent.CompanionTest1"]);
        assert_eq!(*seen_host.lock().unwrap(), Some(true));

        // Host -> companion, and companion -> host.
        let r = host
            .call_method(
                None::<&str>,
                AGENT_PATH,
                Some("fr.ameleo.Agent.CompanionTest1"),
                "Add",
                &(3u32,),
            )
            .await
            .unwrap();
        assert_eq!(r.body().deserialize::<String>().unwrap(), "drive 3");
        let r = comp
            .call_method(
                None::<&str>,
                HOST_PATH,
                Some("fr.ameleo.Host.CompanionTest1"),
                "Read",
                &(4u32,),
            )
            .await
            .unwrap();
        assert_eq!(r.body().deserialize::<Vec<u8>>().unwrap(), [1, 1, 1, 1]);

        // A reset, then at once a call of the next client session: the
        // companion handles the reset (slowly) before the call reaches it.
        me.reset();
        let r = host
            .call_method(
                None::<&str>,
                AGENT_PATH,
                Some("fr.ameleo.Agent.CompanionTest1"),
                "ResetsSeen",
                &(),
            )
            .await
            .unwrap();
        assert_eq!(r.body().deserialize::<u32>().unwrap(), 1);
        assert_eq!(resets.load(Ordering::Acquire), 1);

        // The companion goes away: so do its interfaces.
        drop((register, comp));
        link_task.await.unwrap();
        assert!(channels(&host).await.is_empty());
        let err = host
            .call_method(
                None::<&str>,
                AGENT_PATH,
                Some("fr.ameleo.Agent.CompanionTest1"),
                "Add",
                &(3u32,),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("CompanionTest1"), "{err}");
    }
}

#[cfg(test)]
mod contract {
    use zbus::object_server::Interface;

    #[test]
    fn interface_names_follow_the_protocol() {
        assert_eq!(
            super::LinkIface::name().as_str(),
            proto::companion::iface::LINK
        );
    }
}
