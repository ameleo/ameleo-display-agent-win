// SPDX-License-Identifier: GPL-3.0-or-later

//! D-Bus interfaces exported by the guest agent (see the `proto` crate for
//! the contract).

use std::sync::Arc;

use proto::iface;
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
        [iface::MOUSE].into_iter().map(String::from).collect()
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

pub struct MouseIface(pub Arc<Shared>);

/// `spawn = false`: calls are handled one by one in arrival order. By
/// default zbus runs each call in its own task, so a release could overtake
/// its press on the 2-thread runtime and leave the button down, and moves
/// could jitter. The methods only queue an event: nothing blocks.
#[zbus::interface(name = "fr.ameleo.Agent.Mouse1", spawn = false)]
impl MouseIface {
    fn set_abs_position(&self, head: u32, x: u32, y: u32) {
        tracing::trace!(head, x, y, "mouse move received");
        self.0.desktop.send(Cmd::Move { head, x, y });
    }

    fn press(&self, button: u32) {
        tracing::trace!(button, "mouse press received");
        self.0.desktop.send(Cmd::Button { button, down: true });
    }

    fn release(&self, button: u32) {
        tracing::trace!(button, "mouse release received");
        self.0.desktop.send(Cmd::Button {
            button,
            down: false,
        });
    }

    fn scroll(&self, dx: i32, dy: i32) {
        self.0.desktop.send(Cmd::Scroll { dx, dy });
    }
}
