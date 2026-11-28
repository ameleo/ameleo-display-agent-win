// SPDX-License-Identifier: GPL-3.0-only

//! Client printers (`Printer1`, `Host.Printer1`).
//!
//! Each client printer is a virtual printer of the Print Support App, a
//! separate MSIX package ([`proto::print_link::PACKAGE_FAMILY_NAME`]), on
//! the inbox Microsoft Virtual Print Class Driver: no third-party driver,
//! so they work under Windows protected print. Their URI,
//! `ameleo-display-agent:<slot>`, tells the app's tasks which client
//! printer a job or a capabilities request is for. The agent and the app
//! talk over the pipe of [`proto::print_link`] (see [`link_server`]).
//!
//! Virtual printers are per user, and Windows installs them for the calling
//! process's user only (impersonating the user from SYSTEM is refused): the
//! app, started in the user's session at logon, attaches to the agent
//! (`Request::Attach`) and installs and removes them on its orders
//! (`Command`). At each attachment the agent brings the user's virtual
//! printers in line with its printers.

pub mod app_package;
pub mod jobs;
pub mod link_server;
pub mod ticket;
pub mod xps;

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use proto::Error;
use proto::print_link::{Command, Reply, slot_of_uri, uri_of_slot};
use proto::printer::MAX_PRINTERS;
use proto::printer::MAX_XML;
use zbus::connection::Builder;

use crate::feature::Feature;
use crate::host::Host;
use link_server::{Dialogs, Done};

/// `install`: the Print Support App package, when it is next to the
/// executable.
pub fn install() {
    match app_package::install() {
        Ok(Some(path)) => println!("printer app installed for all users: {}", path.display()),
        Ok(None) => println!(
            "printer app not installed (no {} next to the executable): no client printers",
            app_package::MSIX
        ),
        Err(e) => println!("printer app not installed, no client printers: {e:#}"),
    }
}

/// `uninstall`: the Print Support App package, for all users.
pub fn uninstall() {
    match app_package::uninstall() {
        Ok(true) => println!("printer app removed for all users"),
        Ok(false) => {}
        Err(e) => println!("printer app not removed: {e:#}"),
    }
}

/// The client printers feature: `Printer1`, and the link with the Print
/// Support App.
pub struct ClientPrinters {
    printers: Arc<Printers>,
    dialogs: Arc<Dialogs>,
}

impl ClientPrinters {
    /// Starts the link with the Print Support App.
    pub fn new(host: Host) -> Self {
        let printers = Arc::new(Printers::default());
        let dialogs = Arc::new(Dialogs::default());
        link_server::spawn(printers.clone(), dialogs.clone(), host);
        Self { printers, dialogs }
    }
}

impl Feature for ClientPrinters {
    fn channels(&self) -> Vec<&'static str> {
        vec![proto::iface::PRINTER]
    }

    fn serve<'a>(&self, builder: Builder<'a>) -> zbus::Result<Builder<'a>> {
        Ok(builder.serve_at(
            proto::AGENT_PATH,
            PrinterIface {
                printers: self.printers.clone(),
                dialogs: self.dialogs.clone(),
            },
        ))
    }

    /// The printers are kept a while for a reconnection; the client driver
    /// dialogs waiting fail.
    fn reset(&self) {
        self.printers.reset();
        self.dialogs.clear();
    }

    fn stop(&self) {
        self.printers.remove_all_now();
    }
}

struct PrinterIface {
    printers: Arc<Printers>,
    dialogs: Arc<Dialogs>,
}

#[zbus::interface(name = "fr.ameleo.Agent.Printer1")]
impl PrinterIface {
    /// Returns once Windows created the printer.
    async fn add_printer(
        &self,
        id: u32,
        name: &str,
        is_default: bool,
        capabilities: &str,
        default_ticket: &str,
    ) -> proto::Result<()> {
        tracing::info!(
            id,
            name,
            is_default,
            capabilities = capabilities.len(),
            default_ticket = default_ticket.len(),
            "AddPrinter"
        );
        // The client session of the call: a reset that comes after it
        // removes the printer, one before it does not.
        let generation = self.printers.generation();
        if capabilities.len() > MAX_XML || default_ticket.len() > MAX_XML {
            tracing::warn!(id, "AddPrinter refused: XML over {MAX_XML} bytes");
            return Err(Error::InvalidArgument(format!(
                "capabilities and ticket: at most {MAX_XML} bytes each"
            )));
        }
        let printers = self.printers.clone();
        let (name, capabilities, default_ticket) = (
            name.to_string(),
            capabilities.to_string(),
            default_ticket.to_string(),
        );
        let res = tokio::task::spawn_blocking(move || {
            printers.add(
                generation,
                id,
                &name,
                is_default,
                &capabilities,
                &default_ticket,
            )
        })
        .await
        .map_err(|e| Error::Failed(e.to_string()))?;
        if let Err(e) = &res {
            tracing::warn!(id, "AddPrinter failed: {e}");
        }
        res
    }

    async fn remove_printer(&self, id: u32) {
        tracing::info!(id, "RemovePrinter");
        let printers = self.printers.clone();
        let _ = tokio::task::spawn_blocking(move || printers.remove(id)).await;
    }

    /// The client driver dialog was closed: back to the Print Support App.
    fn document_properties_done(
        &self,
        request_id: u32,
        print_ticket: &str,
        cancelled: bool,
    ) -> proto::Result<()> {
        tracing::info!(
            request_id,
            cancelled,
            ticket = print_ticket.len(),
            "DocumentPropertiesDone"
        );
        let done = Done {
            ticket: print_ticket.to_string(),
            cancelled,
        };
        if self.dialogs.done(request_id, done) {
            Ok(())
        } else {
            Err(Error::NotFound(format!("no pending dialog {request_id}")))
        }
    }
}

/// Slots (URIs) shared by the printers in use and the ones kept for a
/// reconnection.
pub const SLOTS: u16 = 2 * MAX_PRINTERS as u16;
/// The app is not attached: the printers are installed when it attaches.
const NOT_ATTACHED: &str = "the Print Support App is not running for the user";

/// The guest name of a client printer: what Windows accepts (no `\` or
/// `,`, at most 220 characters), marked as redirected like RDP does.
pub fn guest_name(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| {
            if matches!(c, '\\' | ',') || c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect();
    let clean = clean.trim();
    let clean = if clean.is_empty() { "Printer" } else { clean };
    let base: String = clean.chars().take(200).collect();
    format!("{base} (redirected)")
}

// --- the client printers ---------------------------------------------------

/// A printer of the current session.
struct Live {
    guest: String,
    slot: u16,
}

/// A printer of an ended session waiting for a printer of the same name to
/// reuse it, removed after [`PARK_TIME`].
struct Parked {
    slot: u16,
    batch: u64,
}

/// How long the printers of an ended session are kept for the next one: a
/// reconnection gets its printers at once, with the user's settings.
const PARK_TIME: Duration = Duration::from_secs(60);

/// What the host said about a client printer, for the app's tasks.
#[derive(Clone, Default)]
pub struct ClientDriver {
    /// The client driver's PrintCapabilities XML (may be empty).
    pub capabilities: String,
    /// Its default PrintTicket (may be empty).
    pub default_ticket: String,
}

#[derive(Default)]
pub struct Printers {
    /// Host id -> its printer.
    map: Mutex<HashMap<u32, Live>>,
    /// Guest name -> printer kept for reuse.
    parked: Mutex<HashMap<String, Parked>>,
    batches: std::sync::atomic::AtomicU64,
    /// Slot -> the client driver of its printer.
    drivers: Mutex<HashMap<u16, ClientDriver>>,
    /// Printers are added one at a time (slot choice).
    creating: Mutex<()>,
    /// The app attached for the user (its link connection).
    app: Mutex<Option<File>>,
    /// The default printer to set when the app attaches.
    default: Mutex<Option<String>>,
    /// Printers this run installed (lowercase name -> slot): their URI is
    /// known to be right. Another one of the same name has an unknown URI.
    ours: Mutex<HashMap<String, u16>>,
    /// The client session, one more at each reset: a printer that an older
    /// session's `AddPrinter` finishes after the reset is parked, like the
    /// others of its session.
    generation: AtomicU64,
}

impl Printers {
    /// The client session of the moment, for [`Printers::add`].
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// `Agent1.Reset` or a new D-Bus session: the printers are parked for a
    /// reconnection. Never blocks for long: it only takes the map.
    pub fn reset(self: &Arc<Self>) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.remove_all();
    }

    /// A slot no printer uses.
    fn free_slot(&self) -> Option<u16> {
        let map = self.map.lock().unwrap();
        let parked = self.parked.lock().unwrap();
        (0..SLOTS)
            .find(|s| !map.values().any(|l| l.slot == *s) && !parked.values().any(|p| p.slot == *s))
    }

    /// Has the attached app carry out `cmd`; [`NOT_ATTACHED`] if none.
    fn command(&self, cmd: Command) -> Result<(), String> {
        let mut guard = self.app.lock().unwrap();
        let Some(app) = guard.as_mut() else {
            return Err(NOT_ATTACHED.into());
        };
        let res = cmd.write(app).and_then(|()| Reply::read(app));
        match res {
            Ok(Reply::Done) => Ok(()),
            Ok(Reply::Failed(e)) => Err(e),
            Ok(other) => Err(format!("unexpected reply {other:?}")),
            Err(e) => {
                *guard = None;
                tracing::info!("Print Support App detached: {e}");
                Err(format!("Print Support App: {e}"))
            }
        }
    }

    fn install(&self, name: &str, slot: u16) -> Result<(), String> {
        self.command(Command::Install {
            name: name.into(),
            uri: uri_of_slot(slot),
        })?;
        self.ours.lock().unwrap().insert(name.to_lowercase(), slot);
        Ok(())
    }

    /// Not attached: nothing to do now, the app's attachment removes what is
    /// not ours anymore.
    fn uninstall(&self, name: &str) -> Result<(), String> {
        self.ours.lock().unwrap().remove(&name.to_lowercase());
        match self.command(Command::Remove { name: name.into() }) {
            Err(e) if e == NOT_ATTACHED => Ok(()),
            other => other,
        }
    }

    /// The app attached (user logon, app or agent restart): the user's
    /// virtual printers brought in line with ours, in the background.
    pub fn attach(self: &Arc<Self>, app: File, installed: Vec<String>) {
        tracing::info!(installed = installed.len(), "Print Support App attached");
        *self.app.lock().unwrap() = Some(app);
        let this = self.clone();
        let _ = std::thread::Builder::new()
            .name("printers-sync".into())
            .spawn(move || this.sync(installed));
    }

    fn sync(&self, installed: Vec<String>) {
        let wanted: Vec<(String, u16)> = self
            .map
            .lock()
            .unwrap()
            .values()
            .map(|l| (l.guest.clone(), l.slot))
            .chain(
                self.parked
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(n, p)| (n.clone(), p.slot)),
            )
            .collect();
        // Right as they are: the ones this run installed on the same slot.
        let right: HashSet<String> = {
            let ours = self.ours.lock().unwrap();
            installed
                .iter()
                .map(|n| n.to_lowercase())
                .filter(|n| {
                    wanted
                        .iter()
                        .any(|(w, s)| w.to_lowercase() == *n && ours.get(n) == Some(s))
                })
                .collect()
        };
        for name in installed
            .iter()
            .filter(|n| !right.contains(&n.to_lowercase()))
        {
            match self.uninstall(name) {
                Ok(()) => tracing::info!(name, "client printer of a previous run removed"),
                Err(e) => tracing::warn!(name, "{e}"),
            }
        }
        let have = right;
        for (name, slot) in wanted
            .iter()
            .filter(|(n, _)| !have.contains(&n.to_lowercase()))
        {
            match self.install(name, *slot) {
                Ok(()) => tracing::info!(
                    name,
                    uri = uri_of_slot(*slot),
                    "client printer installed for the user"
                ),
                Err(e) => tracing::warn!(name, "{e}"),
            }
        }
        let default = self.default.lock().unwrap().take();
        if let Some(name) = default {
            self.set_default(&name);
        }
    }

    /// Makes `name` the user's default printer, now or when the app attaches.
    fn set_default(&self, name: &str) {
        match self.command(Command::SetDefault { name: name.into() }) {
            Ok(()) => tracing::info!(name, "default printer of the user"),
            Err(e) if e == NOT_ATTACHED => *self.default.lock().unwrap() = Some(name.into()),
            Err(e) => tracing::warn!(name, "cannot make it the default printer: {e}"),
        }
    }

    /// Blocking: the Windows printer of a client printer, for the client
    /// session `generation`. A printer of that name kept from the previous
    /// session is reused; else one is installed.
    pub fn add(
        self: &Arc<Self>,
        generation: u64,
        id: u32,
        name: &str,
        is_default: bool,
        capabilities: &str,
        default_ticket: &str,
    ) -> Result<(), Error> {
        let guest = guest_name(name);
        // One at a time, from the slot choice to the printer being listed:
        // concurrent calls would take the same free slot.
        let _one_at_a_time = self.creating.lock().unwrap_or_else(|e| e.into_inner());
        {
            let map = self.map.lock().unwrap();
            if !map.contains_key(&id) && map.len() >= MAX_PRINTERS as usize {
                return Err(Error::Failed(format!("at most {MAX_PRINTERS} printers")));
            }
        }
        // Same id announced again: the same printer keeps its slot, another
        // one replaces it.
        let previous = self.map.lock().unwrap().remove(&id);
        let mut reusable = None;
        if let Some(old) = previous {
            if old.guest == guest {
                reusable = Some(old.slot);
            } else if let Err(e) = self.uninstall(&old.guest) {
                tracing::warn!(id, name = old.guest, "{e}");
            }
        }
        let reusable =
            reusable.or_else(|| self.parked.lock().unwrap().remove(&guest).map(|p| p.slot));
        let slot = match reusable {
            Some(slot) => {
                tracing::info!(id, name = guest, slot, "client printer reused");
                slot
            }
            None => {
                let slot = self
                    .free_slot()
                    .ok_or_else(|| Error::Failed("no printer slot left".into()))?;
                // One of that name nobody kept (its URI may be another slot's).
                if let Err(e) = self.uninstall(&guest) {
                    tracing::warn!(name = guest, "{e}");
                }
                let t = std::time::Instant::now();
                match self.install(&guest, slot) {
                    Ok(()) => {
                        tracing::info!(id, name = guest, uri = uri_of_slot(slot), took = ?t.elapsed(), "client printer added")
                    }
                    Err(e) if e == NOT_ATTACHED => {
                        tracing::info!(
                            id,
                            name = guest,
                            uri = uri_of_slot(slot),
                            "client printer added: installed when the Print Support App attaches"
                        )
                    }
                    Err(e) => return Err(Error::Failed(format!("{guest}: {e}"))),
                }
                slot
            }
        };
        self.drivers.lock().unwrap().insert(
            slot,
            ClientDriver {
                capabilities: capabilities.to_string(),
                default_ticket: default_ticket.to_string(),
            },
        );
        {
            let mut map = self.map.lock().unwrap();
            // Reset while it was added: the reset parked the printers of
            // the session, so this one goes with them.
            if self.generation() != generation {
                drop(map);
                tracing::info!(
                    id,
                    name = guest,
                    "client printer added after its session ended"
                );
                self.park(vec![(guest, slot)]);
                return Err(Error::Failed("the client session ended".into()));
            }
            map.insert(
                id,
                Live {
                    guest: guest.clone(),
                    slot,
                },
            );
        }
        if is_default {
            self.set_default(&guest);
        }
        Ok(())
    }

    /// The host id of a guest printer, by its Windows name.
    pub fn id_of(&self, guest: &str) -> Option<u32> {
        self.map
            .lock()
            .unwrap()
            .iter()
            .find(|(_, l)| l.guest.eq_ignore_ascii_case(guest))
            .map(|(&id, _)| id)
    }

    /// The printer of a virtual printer URI: `(host id, guest name)`; `None`
    /// for one kept for a reconnection (its printer is gone).
    pub fn by_uri(&self, uri: &str) -> Option<(u32, String)> {
        let slot = slot_of_uri(uri)?;
        self.map
            .lock()
            .unwrap()
            .iter()
            .find(|(_, l)| l.slot == slot)
            .map(|(&id, l)| (id, l.guest.clone()))
    }

    /// The client driver of a virtual printer URI.
    pub fn driver_of(&self, uri: &str) -> Option<ClientDriver> {
        self.drivers
            .lock()
            .unwrap()
            .get(&slot_of_uri(uri)?)
            .cloned()
    }

    /// Blocking: the client printer went away, its printer too.
    pub fn remove(&self, id: u32) {
        let Some(live) = self.map.lock().unwrap().remove(&id) else {
            return;
        };
        self.drivers.lock().unwrap().remove(&live.slot);
        match self.uninstall(&live.guest) {
            Ok(()) => tracing::info!(id, name = live.guest, "client printer removed"),
            Err(e) => tracing::warn!(id, name = live.guest, "{e}"),
        }
    }

    /// Session end: the printers are kept [`PARK_TIME`] for the next session
    /// (a reconnection reuses them), then removed. Printing to them
    /// meanwhile fails.
    pub fn remove_all(self: &Arc<Self>) {
        let printers: Vec<(String, u16)> = self
            .map
            .lock()
            .unwrap()
            .drain()
            .map(|(_, l)| (l.guest, l.slot))
            .collect();
        if !printers.is_empty() {
            tracing::info!(
                printers = printers.len(),
                "client printers kept {PARK_TIME:?} for a reconnection"
            );
            self.park(printers);
        }
    }

    fn park(self: &Arc<Self>, printers: Vec<(String, u16)>) {
        let batch = self
            .batches
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let names: Vec<String> = printers.iter().map(|(g, _)| g.clone()).collect();
        {
            let mut parked = self.parked.lock().unwrap();
            for (guest, slot) in printers {
                parked.insert(guest, Parked { slot, batch });
            }
        }
        let this = self.clone();
        let _ = std::thread::Builder::new()
            .name("printers-parked".into())
            .spawn(move || {
                std::thread::sleep(PARK_TIME);
                for n in names {
                    // Reused, or parked again by a later session: not ours.
                    let expired = {
                        let mut parked = this.parked.lock().unwrap();
                        match parked.get(&n) {
                            Some(p) if p.batch == batch => parked.remove(&n).map(|p| p.slot),
                            _ => None,
                        }
                    };
                    if let Some(slot) = expired {
                        this.drivers.lock().unwrap().remove(&slot);
                        match this.uninstall(&n) {
                            Ok(()) => tracing::info!(name = n, "client printer removed"),
                            Err(e) => tracing::warn!(name = n, "{e}"),
                        }
                    }
                }
            });
    }

    /// Blocking (agent stopping): every printer of this agent, if the app is
    /// attached (else the next attachment removes them).
    pub fn remove_all_now(&self) {
        let names: Vec<String> = self
            .map
            .lock()
            .unwrap()
            .drain()
            .map(|(_, l)| l.guest)
            .chain(self.parked.lock().unwrap().drain().map(|(n, _)| n))
            .collect();
        self.drivers.lock().unwrap().clear();
        if self.app.lock().unwrap().is_none() {
            return;
        }
        for n in names {
            match self.uninstall(&n) {
                Ok(()) => tracing::info!(name = n, "client printer removed"),
                Err(e) => tracing::warn!(name = n, "{e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(
            guest_name("HP LaserJet, 2nd\\floor"),
            "HP LaserJet  2nd floor (redirected)"
        );
        assert_eq!(guest_name("  "), "Printer (redirected)");
        assert_eq!(
            guest_name(&"x".repeat(500)).chars().count(),
            200 + " (redirected)".len()
        );
    }

    #[test]
    fn slots_and_uris() {
        let printers = Printers::default();
        assert_eq!(printers.free_slot(), Some(0));
        printers.map.lock().unwrap().insert(
            5,
            Live {
                guest: "a (redirected)".into(),
                slot: 0,
            },
        );
        printers
            .parked
            .lock()
            .unwrap()
            .insert("b (redirected)".into(), Parked { slot: 1, batch: 0 });
        printers.drivers.lock().unwrap().insert(
            0,
            ClientDriver {
                capabilities: "<pc/>".into(),
                default_ticket: String::new(),
            },
        );
        assert_eq!(printers.free_slot(), Some(2));
        assert_eq!(
            printers.by_uri("ameleo-display-agent:0"),
            Some((5, "a (redirected)".into()))
        );
        assert_eq!(printers.by_uri("ameleo-display-agent:1"), None);
        assert_eq!(
            printers
                .driver_of("ameleo-display-agent:0")
                .map(|d| d.capabilities)
                .as_deref(),
            Some("<pc/>")
        );
    }

    /// The host announces its printers at once, before the app attaches:
    /// each gets its own slot.
    #[test]
    fn concurrent_adds_get_their_own_slots() {
        let printers = Arc::new(Printers::default());
        let threads: Vec<_> = (0..6)
            .map(|id| {
                let p = printers.clone();
                std::thread::spawn(move || {
                    p.add(0, id, &format!("printer {id}"), id == 0, "", "")
                        .unwrap()
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        let slots: HashSet<u16> = printers
            .map
            .lock()
            .unwrap()
            .values()
            .map(|l| l.slot)
            .collect();
        assert_eq!(slots.len(), 6);
        // The default printer waits for the app.
        assert_eq!(
            printers.default.lock().unwrap().as_deref(),
            Some("printer 0 (redirected)")
        );
    }

    /// Session end: the printers are kept for reuse.
    #[test]
    fn session_end_parks_the_printers() {
        let printers = Arc::new(Printers::default());
        let name = "ameleo-display-agent test printer that does not exist (redirected)".to_string();
        printers.map.lock().unwrap().insert(
            5,
            Live {
                guest: name.clone(),
                slot: 7,
            },
        );
        printers.remove_all();
        assert!(printers.map.lock().unwrap().is_empty());
        assert_eq!(
            printers.parked.lock().unwrap().get(&name).map(|p| p.slot),
            Some(7)
        );
    }
}
