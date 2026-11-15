// SPDX-License-Identifier: GPL-3.0-only

//! D-Bus interfaces exported by the guest agent (see the `proto` crate for
//! the contract).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::desktop::{Cmd, DesktopHandle};

pub struct Shared {
    pub desktop: DesktopHandle,
    pub locked: Arc<AtomicBool>,
}

impl Shared {
    pub fn new(desktop: DesktopHandle) -> Arc<Self> {
        Arc::new(Self {
            desktop,
            locked: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Per-session state goes away with the D-Bus session.
    pub fn reset(&self) {
        self.desktop.send(Cmd::Reset);
    }

    /// The D-Bus interfaces of the features, for `Agent1.Channels`.
    fn channels(&self) -> Vec<String> {
        Vec::new()
    }
}

pub struct AgentIface(pub Arc<Shared>);

#[zbus::interface(name = "fr.ameleo.Agent1")]
impl AgentIface {
    #[zbus(property)]
    fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").into()
    }

    #[zbus(property)]
    fn channels(&self) -> Vec<String> {
        self.0.channels()
    }

    #[zbus(property)]
    fn session_locked(&self) -> bool {
        self.0.locked.load(Ordering::Relaxed)
    }

    fn reset(&self) {
        tracing::info!("remote client reset");
        self.0.reset();
        crate::user::lock_session(self.0.locked.load(Ordering::Relaxed));
    }
}
