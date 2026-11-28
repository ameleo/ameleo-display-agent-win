// SPDX-License-Identifier: GPL-3.0-only

//! Client drives (as RDP drive redirection has them), mounted with Dokany.
//!
//! Each client drive is a Dokany file system on a free drive letter. Every
//! operation of a guest app becomes the matching call of `Host.Drive1`, which
//! the host forwards to its client (RDPDR for an RDP one): nothing of the client's files
//! is kept in the guest. The NT parameters of an open go to the client as
//! the app gave them, so the client's file system decides (sharing, existing
//! files, directories), like on a Windows RDP server.
//!
//! The drive letter is global (the agent is SYSTEM), so every open checks
//! that it comes from our session: other sessions and services get access
//! denied.
//!
//! Dokany calls us on its own threads; the calls to the host block them on
//! the agent's runtime.

pub mod dokan;

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use proto::HostDriveProxy;
use proto::drive::{Entry, IO_MAX, MAX_DRIVES, action, information};
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE, NTSTATUS};
use windows_sys::Win32::Security::{GetTokenInformation, TokenSessionId};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, GetLogicalDrives, WIN32_FIND_DATAW,
};
use windows_sys::core::BOOL;
use zbus::Connection;
use zbus::proxy::CacheProperties;

use crate::win::pwstr;
use dokan::{FillFindData, Info, Instance, Name, Notify};

const STATUS_SUCCESS: NTSTATUS = 0;
const STATUS_INVALID_PARAMETER: NTSTATUS = 0xC000_000D_u32 as i32;
const STATUS_END_OF_FILE: NTSTATUS = 0xC000_0011_u32 as i32;
const STATUS_ACCESS_DENIED: NTSTATUS = 0xC000_0022_u32 as i32;
const STATUS_NO_SUCH_FILE: NTSTATUS = 0xC000_000F_u32 as i32;
const STATUS_OBJECT_NAME_NOT_FOUND: NTSTATUS = 0xC000_0034_u32 as i32;
const STATUS_FILE_IS_A_DIRECTORY: NTSTATUS = 0xC000_00BA_u32 as i32;
const STATUS_NOT_A_DIRECTORY: NTSTATUS = 0xC000_0103_u32 as i32;
const STATUS_OBJECT_NAME_COLLISION: NTSTATUS = 0xC000_0035_u32 as i32;
const STATUS_DEVICE_NOT_CONNECTED: NTSTATUS = 0xC000_009D_u32 as i32;
const STATUS_IO_TIMEOUT: NTSTATUS = 0xC000_00B5_u32 as i32;
const STATUS_UNEXPECTED_NETWORK_ERROR: NTSTATUS = 0xC000_00C4_u32 as i32;
const STATUS_INTERNAL_ERROR: NTSTATUS = 0xC000_00E5_u32 as i32;

const FILE_DIRECTORY_FILE: u32 = 0x1;
const FILE_NON_DIRECTORY_FILE: u32 = 0x40;
const FILE_DELETE_ON_CLOSE: u32 = 0x1000;
const FILE_OPEN: u32 = 1;
const FILE_OPEN_IF: u32 = 3;
const FILE_OVERWRITE: u32 = 4;
/// Access that only reads what a listing tells (attributes, times, size).
const INFO_ACCESS: u32 = 0x8 /* FILE_READ_EA */ | 0x80 /* FILE_READ_ATTRIBUTES */ | 0x2_0000 /* READ_CONTROL */
    | 0x10_0000 /* SYNCHRONIZE */;
/// On a folder: listing it (`find_files` lists by path) and going through.
const DIR_ACCESS: u32 = 0x1 /* FILE_LIST_DIRECTORY */ | 0x20 /* FILE_TRAVERSE */;
const FILE_OVERWRITE_IF: u32 = 5;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_CASE_PRESERVED_NAMES: u32 = 0x2;
const FILE_UNICODE_ON_DISK: u32 = 0x4;

/// Longer than a host call may take ([`crate::host::HOST_CALL_TIMEOUT`]):
/// Dokany unmounts a drive whose request times out.
const DOKAN_TIMEOUT_MS: u32 = 120_000;
/// How long what the client said of a file (listing, query) is trusted:
/// changes made on the client side show after that at the latest.
const ENTRY_TTL: Duration = Duration::from_secs(5);
/// How long a path is trusted to stay a folder or a file.
const KIND_TTL: Duration = Duration::from_secs(60);
/// How long the client volume's size is trusted.
const VOLUME_TTL: Duration = Duration::from_secs(10);

/// The D-Bus session with the host, for the threads Dokany calls us on.
pub struct HostLink {
    rt: tokio::runtime::Handle,
    conn: Mutex<Option<Connection>>,
    stats: Mutex<Stats>,
}

/// Host calls per kind since `since`: count, total and longest time.
struct Stats {
    since: Instant,
    calls: HashMap<&'static str, (u32, Duration, Duration)>,
    /// Opens the client refused (name, status, request), with how often.
    failed: HashMap<String, u32>,
    /// Bytes read and written.
    bytes: HashMap<&'static str, u64>,
}

/// How often the host call times are logged (when there were calls).
const STATS_EVERY: Duration = Duration::from_secs(10);

impl HostLink {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            rt: tokio::runtime::Handle::current(),
            conn: Mutex::new(None),
            stats: Mutex::new(Stats {
                since: Instant::now(),
                calls: HashMap::new(),
                failed: HashMap::new(),
                bytes: HashMap::new(),
            }),
        })
    }

    fn record(&self, what: &'static str, took: Duration) {
        let mut s = self.stats.lock().unwrap();
        let e = s.calls.entry(what).or_default();
        e.0 += 1;
        e.1 += took;
        e.2 = e.2.max(took);
        if s.since.elapsed() >= STATS_EVERY {
            let mut calls: Vec<_> = s.calls.drain().collect();
            calls.sort_by_key(|c| std::cmp::Reverse(c.1.1));
            let bytes = std::mem::take(&mut s.bytes);
            let line = calls
                .iter()
                .map(|(w, (n, total, max))| {
                    let size = bytes.get(w).map_or(String::new(), |b| {
                        format!(
                            " {:.1}KiB/call {:.0}KiB/s",
                            *b as f64 / 1024.0 / *n as f64,
                            *b as f64 / 1024.0 / total.as_secs_f64()
                        )
                    });
                    format!(
                        "{w} {n}x avg {:.1}ms max {:.1}ms{size}",
                        total.as_secs_f64() * 1e3 / *n as f64,
                        max.as_secs_f64() * 1e3
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            tracing::info!(
                secs = s.since.elapsed().as_secs(),
                "client drive host calls: {line}"
            );
            if !s.failed.is_empty() {
                let mut failed: Vec<_> = s.failed.drain().collect();
                failed.sort_by_key(|m| std::cmp::Reverse(m.1));
                let total: u32 = failed.iter().map(|m| m.1).sum();
                let top = failed
                    .iter()
                    .take(10)
                    .map(|(n, c)| format!("[{n}] {c}x"))
                    .collect::<Vec<_>>()
                    .join(", ");
                tracing::info!("client drive failed opens: {total}, most: {top}");
            }
            s.since = Instant::now();
        }
    }

    fn bytes(&self, what: &'static str, n: u64) {
        *self.stats.lock().unwrap().bytes.entry(what).or_default() += n;
    }

    /// An open the client refused: what and why.
    fn failed(&self, what: &str) {
        let mut s = self.stats.lock().unwrap();
        if s.failed.len() < 1_000 {
            *s.failed.entry(what.to_lowercase()).or_default() += 1;
        }
    }

    pub fn set(&self, conn: Option<Connection>) {
        *self.conn.lock().unwrap() = conn;
    }

    /// Runs `f` on the host connection and waits for it, from a thread that
    /// is not the runtime's.
    fn call<T, Fut>(
        &self,
        what: &'static str,
        f: impl FnOnce(Connection) -> Fut,
    ) -> Result<T, NTSTATUS>
    where
        Fut: Future<Output = zbus::Result<T>>,
    {
        let Some(conn) = self.conn.lock().unwrap().clone() else {
            return Err(STATUS_DEVICE_NOT_CONNECTED);
        };
        // The timeout is made inside: its timer needs the runtime's context,
        // which Dokany's threads do not have.
        let call =
            async move { tokio::time::timeout(crate::host::HOST_CALL_TIMEOUT, f(conn)).await };
        let start = Instant::now();
        let r = self.rt.block_on(call);
        self.record(what, start.elapsed());
        match r {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => {
                tracing::warn!("client drive {what}: {e}");
                Err(STATUS_UNEXPECTED_NETWORK_ERROR)
            }
            Err(_) => {
                tracing::warn!("client drive {what}: no host answer");
                Err(STATUS_IO_TIMEOUT)
            }
        }
    }
}

/// `Host.Drive1` on the connection, inside a [`HostLink::call`].
macro_rules! host {
    ($fs:expr, $what:expr, |$p:ident| $body:expr) => {
        $fs.host.call($what, |conn| async move {
            let $p = HostDriveProxy::builder(&conn)
                .cache_properties(CacheProperties::No)
                .build()
                .await?;
            $body.await
        })
    };
}

/// A folder listing and when the client gave it.
type Listing = (Arc<Vec<Entry>>, Instant);

/// One mounted client drive: what its callbacks need.
struct DriveFs {
    id: u32,
    label: String,
    host: Arc<HostLink>,
    session: u32,
    /// What the client said lately of files (listings, queries), by
    /// lowercase path: Explorer asks for each file of a folder it lists.
    entries: Mutex<HashMap<String, (Entry, Instant)>>,
    /// Folder listings the client gave lately, by lowercase path: Explorer
    /// lists a folder several times, and each costs one client round trip
    /// per file (RDPDR gives one entry per request).
    listings: Mutex<HashMap<String, Listing>>,
    /// Folder or file, by lowercase path, kept longer than the rest: a copy
    /// opens each folder of the path of each file as a file.
    kinds: Mutex<HashMap<String, (bool, Instant)>>,
    /// `(when, total, free, file system)`.
    volume: Mutex<Option<(Instant, u64, u64, String)>>,
}

struct Mount {
    /// The client session that added it (see [`Drives::generation`]).
    generation: u64,
    letter: char,
    instance: Instance,
    fs: *const DriveFs,
    // Dokany keeps pointers to these while mounted.
    _options: Box<dokan::Options>,
    _operations: Box<dokan::Operations>,
    _mount_point: Vec<u16>,
}

// The raw pointers are Dokany's and ours, used from any thread.
unsafe impl Send for Mount {}

#[derive(Default)]
pub struct Drives {
    mounts: Mutex<HashMap<u32, Mount>>,
    /// The client session, one more at each reset. A reset must not wait
    /// for a mount in progress, so it only counts and leaves the unmounting
    /// to a thread, which removes the drives of the sessions before it and
    /// leaves the next session's alone, however late it runs.
    generation: AtomicU64,
}

impl Drives {
    /// The client session of the moment, for [`Drives::add`].
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Mounts the client drive `id` for the client session `generation`;
    /// returns its letter.
    pub fn add(
        &self,
        host: &Arc<HostLink>,
        generation: u64,
        id: u32,
        label: &str,
    ) -> Result<char, String> {
        let api = dokan::api()?;
        let mut mounts = self.mounts.lock().unwrap();
        match mounts.get(&id) {
            Some(m) if m.generation == generation => return Ok(m.letter),
            // A drive of a session reset since: it goes now.
            Some(_) => {
                if let Some(m) = mounts.remove(&id) {
                    unmount(m);
                }
            }
            None => {}
        }
        if mounts.len() >= MAX_DRIVES as usize {
            return Err(format!("at most {MAX_DRIVES} client drives"));
        }
        let used = unsafe { GetLogicalDrives() };
        let letter = ('D'..='Z')
            .rev()
            .find(|c| {
                used & (1 << (*c as u32 - 'A' as u32)) == 0
                    && !mounts.values().any(|m| m.letter == *c)
            })
            .ok_or("no free drive letter")?;
        let fs = Arc::into_raw(Arc::new(DriveFs {
            id,
            label: label.to_string(),
            host: host.clone(),
            session: crate::user::current_session(),
            entries: Mutex::default(),
            listings: Mutex::default(),
            kinds: Mutex::default(),
            volume: Mutex::default(),
        }));
        let mount_point = crate::win::wide(format!("{letter}:\\"));
        // SAFETY: zeroed is valid for this plain C struct (the 16 KiB
        // descriptor array included).
        let mut options: Box<dokan::Options> = Box::new(unsafe { std::mem::zeroed() });
        options.version = dokan::VERSION;
        options.global_context = fs as u64;
        options.mount_point = mount_point.as_ptr();
        options.timeout = DOKAN_TIMEOUT_MS;
        let mut operations = Box::new(operations());
        match api.mount(&mut options, &mut operations) {
            Ok(instance) => {
                let m = Mount {
                    generation,
                    letter,
                    instance,
                    fs,
                    _options: options,
                    _operations: operations,
                    _mount_point: mount_point,
                };
                // Reset while it mounted: the reset's thread may have run
                // already, so the drive goes here.
                if self.generation() != generation {
                    unmount(m);
                    return Err("the client session ended".into());
                }
                tracing::info!(id, %letter, label, "client drive mounted");
                mounts.insert(id, m);
                Ok(letter)
            }
            Err(e) => {
                drop(unsafe { Arc::from_raw(fs) });
                Err(format!("Dokany could not mount {letter}: (error {e})"))
            }
        }
    }

    /// `Agent1.Reset` or a new D-Bus session: the drives added so far go,
    /// on a thread, since unmounting waits for the operations running.
    pub fn reset(self: &Arc<Self>) {
        let next = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let drives = self.clone();
        std::thread::spawn(move || drives.remove_before(next));
    }

    /// Unmounts the drives added before the client session `generation`.
    fn remove_before(&self, generation: u64) {
        let old: Vec<Mount> = {
            let mut mounts = self.mounts.lock().unwrap();
            let ids: Vec<u32> = mounts
                .iter()
                .filter(|(_, m)| m.generation < generation)
                .map(|(id, _)| *id)
                .collect();
            ids.iter().filter_map(|id| mounts.remove(id)).collect()
        };
        for m in old {
            unmount(m);
        }
    }

    pub fn remove(&self, id: u32) {
        let m = self.mounts.lock().unwrap().remove(&id);
        if let Some(m) = m {
            unmount(m);
        }
    }

    pub fn remove_all(&self) {
        let all: Vec<Mount> = self
            .mounts
            .lock()
            .unwrap()
            .drain()
            .map(|(_, m)| m)
            .collect();
        for m in all {
            unmount(m);
        }
    }

    /// Changes made on the client: Explorer is told, what we know of the
    /// paths forgotten.
    pub fn changed(&self, id: u32, changes: &[(u32, String)]) {
        let Ok(api) = dokan::api() else { return };
        let mounts = self.mounts.lock().unwrap();
        let Some(m) = mounts.get(&id) else { return };
        let fs = unsafe { &*m.fs };
        let mut old: Option<String> = None;
        for (act, path) in changes {
            let full = format!("\\{}", path.trim_start_matches('\\'));
            let dir = fs
                .known(path)
                .is_some_and(|e| e.1 & FILE_ATTRIBUTE_DIRECTORY != 0);
            fs.forget(path);
            match *act {
                action::ADDED => api.notify(m.instance, Notify::Create { path: &full, dir }),
                action::REMOVED => api.notify(m.instance, Notify::Delete { path: &full, dir }),
                action::MODIFIED => api.notify(m.instance, Notify::Update { path: &full }),
                action::RENAMED_OLD_NAME => old = Some(full),
                action::RENAMED_NEW_NAME => match old.take() {
                    Some(o) => api.notify(
                        m.instance,
                        Notify::Rename {
                            old: &o,
                            new: &full,
                            dir,
                        },
                    ),
                    None => api.notify(m.instance, Notify::Create { path: &full, dir }),
                },
                _ => {}
            }
        }
    }
}

/// Unmounts (waits for the callbacks running) and frees the drive.
fn unmount(m: Mount) {
    if let Ok(api) = dokan::api() {
        api.unmount(m.instance);
    }
    tracing::info!(letter = %m.letter, "client drive unmounted");
    drop(unsafe { Arc::from_raw(m.fs) });
}

/// What Dokany calls: the callback below, a panic caught. A panic must not
/// cross into Dokany (the agent would abort, the drive with it): the
/// operation fails instead.
macro_rules! guarded {
    ($($name:ident($($a:ident: $t:ty),*) $(-> $ret:ty = $fail:expr)?;)*) => {
        mod guarded {
            #[allow(unused_imports)]
            use super::*;
            $(
                pub unsafe extern "system" fn $name($($a: $t),*) $(-> $ret)? {
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe { super::$name($($a),*) }));
                    match r {
                        Ok(v) => v,
                        Err(_) => {
                            tracing::error!(concat!("client drive ", stringify!($name), " panicked"));
                            $($fail)?
                        }
                    }
                }
            )*
        }
    };
}

guarded! {
    create_file(name: Name, security: *mut std::ffi::c_void, access: u32, attributes: u32, share: u32, disposition: u32, options: u32, info: Info) -> NTSTATUS = STATUS_INTERNAL_ERROR;
    cleanup(name: Name, info: Info);
    close_file(name: Name, info: Info);
    read_file(name: Name, buffer: *mut std::ffi::c_void, length: u32, read: *mut u32, offset: i64, info: Info) -> NTSTATUS = STATUS_INTERNAL_ERROR;
    write_file(name: Name, buffer: *const std::ffi::c_void, length: u32, written: *mut u32, offset: i64, info: Info) -> NTSTATUS = STATUS_INTERNAL_ERROR;
    flush_file_buffers(name: Name, info: Info) -> NTSTATUS = STATUS_INTERNAL_ERROR;
    get_file_information(name: Name, out: *mut BY_HANDLE_FILE_INFORMATION, info: Info) -> NTSTATUS = STATUS_INTERNAL_ERROR;
    find_files(name: Name, fill: FillFindData, info: Info) -> NTSTATUS = STATUS_INTERNAL_ERROR;
    set_file_attributes(name: Name, attributes: u32, info: Info) -> NTSTATUS = STATUS_INTERNAL_ERROR;
    set_file_time(name: Name, creation: *const u64, access: *const u64, write: *const u64, info: Info) -> NTSTATUS = STATUS_INTERNAL_ERROR;
    delete(name: Name, info: Info) -> NTSTATUS = STATUS_INTERNAL_ERROR;
    move_file(name: Name, new_name: Name, replace: BOOL, info: Info) -> NTSTATUS = STATUS_INTERNAL_ERROR;
    set_end_of_file(name: Name, size: i64, info: Info) -> NTSTATUS = STATUS_INTERNAL_ERROR;
    set_allocation_size(name: Name, size: i64, info: Info) -> NTSTATUS = STATUS_INTERNAL_ERROR;
    get_disk_free_space(free_available: *mut u64, total: *mut u64, total_free: *mut u64, info: Info) -> NTSTATUS = STATUS_INTERNAL_ERROR;
    get_volume_information(name: *mut u16, name_size: u32, serial: *mut u32, max_component: *mut u32, flags: *mut u32, fs_name: *mut u16, fs_name_size: u32, info: Info) -> NTSTATUS = STATUS_INTERNAL_ERROR;
}

fn operations() -> dokan::Operations {
    use guarded::*;
    dokan::Operations {
        create_file: Some(create_file),
        cleanup: Some(cleanup),
        close_file: Some(close_file),
        read_file: Some(read_file),
        write_file: Some(write_file),
        flush_file_buffers: Some(flush_file_buffers),
        get_file_information: Some(get_file_information),
        find_files: Some(find_files),
        set_file_attributes: Some(set_file_attributes),
        set_file_time: Some(set_file_time),
        delete_file: Some(delete),
        delete_directory: Some(delete),
        move_file: Some(move_file),
        set_end_of_file: Some(set_end_of_file),
        set_allocation_size: Some(set_allocation_size),
        get_disk_free_space: Some(get_disk_free_space),
        get_volume_information: Some(get_volume_information),
        // Byte range locks: Dokany's driver keeps them. Security: Dokany's
        // default descriptor; the session check is ours.
        ..Default::default()
    }
}

impl DriveFs {
    /// What the client said of `path` lately.
    fn known(&self, path: &str) -> Option<Entry> {
        let cache = self.entries.lock().unwrap();
        cache
            .get(&path.to_lowercase())
            .filter(|(_, at)| at.elapsed() < ENTRY_TTL)
            .map(|(e, _)| e.clone())
    }

    fn remember(&self, path: &str, entry: Entry) {
        self.set_kind(path, entry.1 & FILE_ATTRIBUTE_DIRECTORY != 0);
        let mut cache = self.entries.lock().unwrap();
        if cache.len() > 10_000 {
            cache.retain(|_, (_, at)| at.elapsed() < ENTRY_TTL);
        }
        cache.insert(path.to_lowercase(), (entry, Instant::now()));
    }

    /// Whether `path` is a folder, as the client said lately.
    fn kind(&self, path: &str) -> Option<bool> {
        if path.is_empty() {
            return Some(true);
        }
        let kinds = self.kinds.lock().unwrap();
        kinds
            .get(&path.to_lowercase())
            .filter(|(_, at)| at.elapsed() < KIND_TTL)
            .map(|(d, _)| *d)
    }

    fn set_kind(&self, path: &str, dir: bool) {
        let mut kinds = self.kinds.lock().unwrap();
        if kinds.len() > 10_000 {
            kinds.retain(|_, (_, at)| at.elapsed() < KIND_TTL);
        }
        kinds.insert(path.to_lowercase(), (dir, Instant::now()));
    }

    /// `path` changed: what we knew of it, of its folder's listing, and of
    /// its own listing (a folder) is stale.
    fn forget(&self, path: &str) {
        let key = path.to_lowercase();
        self.entries.lock().unwrap().remove(&key);
        self.kinds.lock().unwrap().remove(&key);
        let mut listings = self.listings.lock().unwrap();
        listings.remove(&key);
        listings.remove(parent(&key));
    }

    /// The listing of `dir` the client gave lately.
    fn listing(&self, dir: &str) -> Option<Arc<Vec<Entry>>> {
        let listings = self.listings.lock().unwrap();
        listings
            .get(&dir.to_lowercase())
            .filter(|(_, at)| at.elapsed() < ENTRY_TTL)
            .map(|(l, _)| l.clone())
    }

    /// Lists `dir` on the client; what it says of each file is remembered.
    fn list(&self, dir: &str) -> Result<Arc<Vec<Entry>>, NTSTATUS> {
        if let Some(l) = self.listing(dir) {
            return Ok(l);
        }
        let id = self.id;
        let d = dir.to_string();
        let (st, entries) = host!(self, "list", |p| p.list(id, &d))?;
        if st != STATUS_SUCCESS as u32 {
            return Err(status(st));
        }
        // Stamped before the entries: they outlive it, so a name missing
        // from a fresh listing is missing (see `absent`).
        let at = Instant::now();
        let entries: Vec<Entry> = entries
            .into_iter()
            .filter(|e| !(e.0.is_empty() || e.0 == "." || e.0 == ".." || e.0.contains(['\\', '/'])))
            .collect();
        for e in &entries {
            self.remember(
                &if dir.is_empty() {
                    e.0.clone()
                } else {
                    format!("{dir}\\{}", e.0)
                },
                e.clone(),
            );
        }
        let entries = Arc::new(entries);
        let mut listings = self.listings.lock().unwrap();
        if listings.len() > 1_000 {
            listings.retain(|_, (_, at)| at.elapsed() < ENTRY_TTL);
        }
        listings.insert(dir.to_lowercase(), (entries.clone(), at));
        Ok(entries)
    }

    /// `path` is not in the fresh listing of its folder: no need to ask the
    /// client (Explorer looks for `desktop.ini` and others in each folder).
    fn absent(&self, path: &str) -> bool {
        !path.is_empty()
            && !path.contains(':') // a stream: not in listings
            && self.listing(parent(path)).is_some()
            && self.known(path).is_none()
    }

    /// Opens `path` on the client: `(file id, information)`.
    fn open(&self, path: &str, o: &OpenParams, disposition: u32) -> Result<(u32, u32), NTSTATUS> {
        let id = self.id;
        let p = path.to_string();
        let o = *o;
        let (st, file_id, info_code) = host!(self, "open", |h| h.open(
            id,
            &p,
            o.access,
            o.attributes,
            o.share,
            disposition,
            o.options
        ))?;
        if st != STATUS_SUCCESS as u32 {
            let name = path.rsplit('\\').next().unwrap_or(path);
            self.host.failed(&format!(
                "{name} {st:#x} access {:#x} disposition {disposition} options {:#x}",
                o.access, o.options
            ));
            return Err(status(st));
        }
        Ok((file_id, info_code))
    }

    /// The operation comes from a thread of our session.
    fn is_from_our_session(&self, info: Info) -> bool {
        let Ok(api) = dokan::api() else { return false };
        let token = api.requestor_token(info);
        if token.is_null() || token == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut session = u32::MAX;
        let mut len = 0u32;
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenSessionId,
                (&mut session as *mut u32).cast(),
                4,
                &mut len,
            )
        };
        unsafe { CloseHandle(token) };
        ok != 0 && session == self.session
    }

    fn query(&self, file: u32) -> Result<Entry, NTSTATUS> {
        let id = self.id;
        let (status, entry) = host!(self, "query", |p| p.query(id, file))?;
        if status != STATUS_SUCCESS as u32 {
            return Err(status as NTSTATUS);
        }
        Ok(entry)
    }

    fn volume(&self) -> Result<(u64, u64, String), NTSTATUS> {
        if let Some((at, total, free, fs)) = self.volume.lock().unwrap().clone()
            && at.elapsed() < VOLUME_TTL
        {
            return Ok((total, free, fs));
        }
        let id = self.id;
        let (status, total, free, _label, fs) = host!(self, "volume", |p| p.volume(id))?;
        if status != STATUS_SUCCESS as u32 {
            return Err(status as NTSTATUS);
        }
        *self.volume.lock().unwrap() = Some((Instant::now(), total, free, fs.clone()));
        Ok((total, free, fs))
    }
}

// --- Dokany callbacks -------------------------------------------------------

/// The drive of a callback.
unsafe fn fs<'a>(info: Info) -> &'a DriveFs {
    unsafe { &*((*(*info).options).global_context as *const DriveFs) }
}

/// What an app's open asked, to open on the client later.
#[derive(Clone, Copy)]
struct OpenParams {
    access: u32,
    attributes: u32,
    share: u32,
    options: u32,
}

/// An open of an app (`context` points to it, boxed).
struct Handle {
    /// The client's file id; `None`: not opened on the client yet.
    file: Mutex<Option<u32>>,
    open: OpenParams,
    /// The file was changed through this open.
    modified: AtomicBool,
}

fn set_handle(info: Info, file: Option<u32>, open: OpenParams) {
    let h = Box::new(Handle {
        file: Mutex::new(file),
        open,
        modified: AtomicBool::new(false),
    });
    unsafe { (*info).context = Box::into_raw(h) as u64 };
}

unsafe fn handle<'a>(info: Info) -> Option<&'a Handle> {
    let c = unsafe { (*info).context };
    (c != 0).then(|| unsafe { &*(c as *const Handle) })
}

/// The client's file id of the open, opening it on the client first if
/// that was put off.
unsafe fn file(fs: &DriveFs, name: Name, info: Info) -> Result<u32, NTSTATUS> {
    let h = unsafe { handle(info) }.ok_or(STATUS_INVALID_PARAMETER)?;
    let mut file = h.file.lock().unwrap();
    if let Some(id) = *file {
        return Ok(id);
    }
    let (id, _) = fs.open(&unsafe { rel(name) }, &h.open, FILE_OPEN)?;
    *file = Some(id);
    Ok(id)
}

/// The file changes through this open: what we knew of it is stale, now and
/// when it is closed.
unsafe fn modified(fs: &DriveFs, name: Name, info: Info) {
    if let Some(h) = unsafe { handle(info) } {
        h.modified.store(true, Ordering::Relaxed);
    }
    fs.forget(&unsafe { rel(name) });
}

/// `dir` of `dir\name` (`""`: the root).
fn parent(path: &str) -> &str {
    path.rsplit_once('\\').map_or("", |p| p.0)
}

/// `\dir\file` as Dokany gives it, to the contract's `dir\file`.
unsafe fn rel(name: Name) -> String {
    pwstr(name).trim_start_matches('\\').to_string()
}

fn status(s: u32) -> NTSTATUS {
    s as NTSTATUS
}

fn filetime(t: u64) -> FILETIME {
    FILETIME {
        dwLowDateTime: t as u32,
        dwHighDateTime: (t >> 32) as u32,
    }
}

// The arguments of Dokany's ZwCreateFile callback.
#[allow(clippy::too_many_arguments)]
unsafe fn create_file(
    name: Name,
    _security: *mut std::ffi::c_void,
    desired_access: u32,
    file_attributes: u32,
    share_access: u32,
    disposition: u32,
    options: u32,
    info: Info,
) -> NTSTATUS {
    let fs = unsafe { fs(info) };
    if !fs.is_from_our_session(info) {
        return STATUS_ACCESS_DENIED;
    }
    let path = unsafe { rel(name) };
    let params = OpenParams {
        access: desired_access,
        attributes: file_attributes,
        share: share_access,
        options,
    };
    let must_exist = disposition == FILE_OPEN || disposition == FILE_OVERWRITE;
    if must_exist && fs.absent(&path) {
        return STATUS_OBJECT_NAME_NOT_FOUND;
    }
    // A file asked where we know a folder, or the other way round (the
    // shell tries the folders it shows as files).
    let kind = fs.kind(&path);
    if disposition == FILE_OPEN {
        match kind {
            Some(true) if options & FILE_NON_DIRECTORY_FILE != 0 => {
                return STATUS_FILE_IS_A_DIRECTORY;
            }
            Some(false) if options & FILE_DIRECTORY_FILE != 0 => return STATUS_NOT_A_DIRECTORY,
            _ => {}
        }
    }
    // An open for the file's information only (Explorer, for each file it
    // shows) is not made on the client while we know the file: it is when
    // something needs it (`file`).
    let known = path.is_empty() || fs.known(&path).is_some();
    if disposition == FILE_OPEN
        && options & FILE_DELETE_ON_CLOSE == 0
        && known
        && let Some(dir) = kind
    {
        let allowed = if dir {
            INFO_ACCESS | DIR_ACCESS
        } else {
            INFO_ACCESS
        };
        if desired_access & !allowed == 0 {
            set_handle(info, None, params);
            unsafe { (*info).is_directory = dir as u8 };
            return STATUS_SUCCESS;
        }
    }
    let (file_id, info_code) = match fs.open(&path, &params, disposition) {
        Ok(v) => v,
        // What NTFS says of a missing file at open: some clients answer
        // with the "no more files" of listings.
        Err(STATUS_NO_SUCH_FILE) => return STATUS_OBJECT_NAME_NOT_FOUND,
        Err(e) => {
            match e {
                STATUS_FILE_IS_A_DIRECTORY => fs.set_kind(&path, true),
                STATUS_NOT_A_DIRECTORY => fs.set_kind(&path, false),
                _ => {}
            }
            return e;
        }
    };
    set_handle(info, Some(file_id), params);
    if info_code != information::OPENED {
        fs.forget(&path); // created, overwritten or superseded
    }
    // Dokany must know a directory from a file.
    let dir = if path.is_empty() || options & FILE_DIRECTORY_FILE != 0 {
        true
    } else if options & FILE_NON_DIRECTORY_FILE != 0 {
        false
    } else if let Some(dir) = fs.kind(&path) {
        dir
    } else {
        match fs.query(file_id) {
            Ok(e) => {
                let dir = e.1 & FILE_ATTRIBUTE_DIRECTORY != 0;
                fs.remember(&path, e);
                dir
            }
            Err(_) => false,
        }
    };
    fs.set_kind(&path, dir);
    unsafe { (*info).is_directory = dir as u8 };
    // Opened, not created: Dokany wants to know for OPEN_IF / OVERWRITE_IF.
    if (disposition == FILE_OPEN_IF || disposition == FILE_OVERWRITE_IF)
        && !information::created(info_code)
    {
        return STATUS_OBJECT_NAME_COLLISION;
    }
    STATUS_SUCCESS
}

/// The client deletes a file marked for deletion when it is closed
/// ([`close_file`]); paging I/O may still come before that.
unsafe fn cleanup(_name: Name, _info: Info) {}

unsafe fn close_file(name: Name, info: Info) {
    let fs = unsafe { fs(info) };
    let c = unsafe { std::mem::replace(&mut (*info).context, 0) };
    if c == 0 {
        return;
    }
    let h = unsafe { Box::from_raw(c as *mut Handle) };
    let changed = h.modified.load(Ordering::Relaxed) || unsafe { (*info).delete_pending != 0 };
    if let Some(file_id) = *h.file.lock().unwrap() {
        let id = fs.id;
        match host!(fs, "close", |p| p.close(id, file_id)) {
            Ok(0) => {}
            Ok(st) => tracing::debug!(status = format!("{st:#x}"), "client drive close"),
            Err(_) => {}
        }
    }
    // The client applies some changes at close (deletion, times).
    if changed {
        fs.forget(&unsafe { rel(name) });
    }
}

unsafe fn read_file(
    name: Name,
    buffer: *mut std::ffi::c_void,
    length: u32,
    read: *mut u32,
    offset: i64,
    info: Info,
) -> NTSTATUS {
    let fs = unsafe { fs(info) };
    let file_id = match unsafe { file(fs, name, info) } {
        Ok(f) => f,
        Err(e) => return e,
    };
    let id = fs.id;
    let mut done = 0u32;
    while done < length {
        let n = (length - done).min(IO_MAX);
        let at = offset as u64 + done as u64;
        let (st, data) = match host!(fs, "read", |p| p.read(id, file_id, at, n)) {
            Ok(v) => v,
            Err(e) => return e,
        };
        if st == STATUS_END_OF_FILE as u32 {
            break;
        }
        if st != STATUS_SUCCESS as u32 {
            if done > 0 {
                break; // what was read is the answer
            }
            return status(st);
        }
        let got = data.len().min(n as usize);
        fs.host.bytes("read", got as u64);
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                buffer.cast::<u8>().add(done as usize),
                got,
            )
        };
        done += got as u32;
        if (got as u32) < n {
            break; // end of file
        }
    }
    unsafe { *read = done };
    STATUS_SUCCESS
}

unsafe fn write_file(
    name: Name,
    buffer: *const std::ffi::c_void,
    length: u32,
    written: *mut u32,
    offset: i64,
    info: Info,
) -> NTSTATUS {
    let fs = unsafe { fs(info) };
    let file_id = match unsafe { file(fs, name, info) } {
        Ok(f) => f,
        Err(e) => return e,
    };
    unsafe { modified(fs, name, info) };
    let (to_end, paging) = unsafe { ((*info).write_to_end_of_file != 0, (*info).paging_io != 0) };
    let mut offset = offset as u64;
    let mut length = length;
    if to_end || paging {
        let size = match fs.query(file_id) {
            Ok(e) => e.2,
            Err(e) => return e,
        };
        if to_end {
            offset = size;
        } else {
            // Paging I/O never extends the file.
            if offset >= size {
                unsafe { *written = 0 };
                return STATUS_SUCCESS;
            }
            // In u64: the rest of the file may be 4 GiB or more.
            length = (length as u64).min(size - offset) as u32;
        }
    }
    let id = fs.id;
    let mut done = 0u32;
    while done < length {
        let n = (length - done).min(IO_MAX);
        let data = unsafe {
            std::slice::from_raw_parts(buffer.cast::<u8>().add(done as usize), n as usize)
        }
        .to_vec();
        let at = offset + done as u64;
        let (st, count) = match host!(fs, "write", |p| p.write(id, file_id, at, data)) {
            Ok(v) => v,
            Err(e) => return e,
        };
        if st != STATUS_SUCCESS as u32 {
            return status(st);
        }
        fs.host.bytes("write", count.min(n) as u64);
        done += count.min(n);
        if count < n {
            break;
        }
    }
    unsafe { *written = done };
    STATUS_SUCCESS
}

unsafe fn flush_file_buffers(_name: Name, info: Info) -> NTSTATUS {
    let fs = unsafe { fs(info) };
    // Not opened on the client: nothing was written.
    let Some(file_id) = (unsafe { handle(info) }).and_then(|h| *h.file.lock().unwrap()) else {
        return STATUS_SUCCESS;
    };
    let id = fs.id;
    host!(fs, "flush", |p| p.flush(id, file_id)).map_or_else(|e| e, status)
}

unsafe fn get_file_information(
    name: Name,
    out: *mut BY_HANDLE_FILE_INFORMATION,
    info: Info,
) -> NTSTATUS {
    let fs = unsafe { fs(info) };
    // Explorer asks for each file of a folder it just listed.
    let path = unsafe { rel(name) };
    let e = match fs.known(&path) {
        Some(e) => e,
        None => {
            let file_id = match unsafe { file(fs, name, info) } {
                Ok(f) => f,
                Err(e) => return e,
            };
            match fs.query(file_id) {
                Ok(e) => {
                    fs.remember(&path, e.clone());
                    e
                }
                Err(e) => return e,
            }
        }
    };
    let out = unsafe { &mut *out };
    out.dwFileAttributes = e.1;
    out.ftCreationTime = filetime(e.3);
    out.ftLastAccessTime = filetime(e.4);
    out.ftLastWriteTime = filetime(e.5);
    out.dwVolumeSerialNumber = fs.id;
    out.nFileSizeHigh = (e.2 >> 32) as u32;
    out.nFileSizeLow = e.2 as u32;
    out.nNumberOfLinks = 1;
    out.nFileIndexHigh = 0;
    out.nFileIndexLow = 0;
    STATUS_SUCCESS
}

unsafe fn find_files(name: Name, fill: FillFindData, info: Info) -> NTSTATUS {
    let fs = unsafe { fs(info) };
    let entries = match fs.list(&unsafe { rel(name) }) {
        Ok(v) => v,
        Err(e) => return e,
    };
    for e in entries.iter() {
        // SAFETY: zeroed is a valid WIN32_FIND_DATAW.
        let mut fd: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
        fd.dwFileAttributes = e.1;
        fd.ftCreationTime = filetime(e.3);
        fd.ftLastAccessTime = filetime(e.4);
        fd.ftLastWriteTime = filetime(e.5);
        fd.nFileSizeHigh = (e.2 >> 32) as u32;
        fd.nFileSizeLow = e.2 as u32;
        let w: Vec<u16> = e.0.encode_utf16().take(fd.cFileName.len() - 1).collect();
        fd.cFileName[..w.len()].copy_from_slice(&w);
        unsafe { fill(&mut fd, info) };
    }
    STATUS_SUCCESS
}

unsafe fn set_file_attributes(name: Name, attributes: u32, info: Info) -> NTSTATUS {
    let fs = unsafe { fs(info) };
    let file_id = match unsafe { file(fs, name, info) } {
        Ok(f) => f,
        Err(e) => return e,
    };
    unsafe { modified(fs, name, info) };
    if attributes == 0 {
        return STATUS_SUCCESS; // nothing to change
    }
    let id = fs.id;
    host!(fs, "set attributes", |p| p
        .set_basic(id, file_id, attributes, 0, 0, 0))
    .map_or_else(|e| e, status)
}

unsafe fn set_file_time(
    name: Name,
    creation: *const u64,
    access: *const u64,
    write: *const u64,
    info: Info,
) -> NTSTATUS {
    let fs = unsafe { fs(info) };
    let file_id = match unsafe { file(fs, name, info) } {
        Ok(f) => f,
        Err(e) => return e,
    };
    unsafe { modified(fs, name, info) };
    // FILETIME pointers, 4-byte aligned; null: unchanged.
    let t = |p: *const u64| {
        if p.is_null() {
            0
        } else {
            unsafe { p.read_unaligned() }
        }
    };
    let (c, a, w) = (t(creation), t(access), t(write));
    let id = fs.id;
    host!(fs, "set times", |p| p.set_basic(id, file_id, 0, c, a, w)).map_or_else(|e| e, status)
}

/// `DeleteFile` and `DeleteDirectory`: the deletion is marked (or unmarked)
/// on the client's open file, which deletes it when closed; a non-empty
/// directory is refused there.
unsafe fn delete(name: Name, info: Info) -> NTSTATUS {
    let fs = unsafe { fs(info) };
    let file_id = match unsafe { file(fs, name, info) } {
        Ok(f) => f,
        Err(e) => return e,
    };
    let pending = unsafe { (*info).delete_pending != 0 };
    unsafe { modified(fs, name, info) };
    let id = fs.id;
    host!(fs, "delete", |p| p.set_delete(id, file_id, pending)).map_or_else(|e| e, status)
}

unsafe fn move_file(name: Name, new_name: Name, replace: BOOL, info: Info) -> NTSTATUS {
    let fs = unsafe { fs(info) };
    let file_id = match unsafe { file(fs, name, info) } {
        Ok(f) => f,
        Err(e) => return e,
    };
    let new = unsafe { rel(new_name) };
    unsafe { modified(fs, name, info) };
    fs.forget(&new);
    let id = fs.id;
    host!(fs, "rename", |p| p.rename(id, file_id, &new, replace != 0)).map_or_else(|e| e, status)
}

unsafe fn set_end_of_file(name: Name, size: i64, info: Info) -> NTSTATUS {
    let fs = unsafe { fs(info) };
    let file_id = match unsafe { file(fs, name, info) } {
        Ok(f) => f,
        Err(e) => return e,
    };
    unsafe { modified(fs, name, info) };
    let id = fs.id;
    host!(fs, "set size", |p| p.set_size(
        id,
        file_id,
        size as u64,
        false
    ))
    .map_or_else(|e| e, status)
}

/// A hint: a client that does not take it is no error.
unsafe fn set_allocation_size(name: Name, size: i64, info: Info) -> NTSTATUS {
    let fs = unsafe { fs(info) };
    let file_id = match unsafe { file(fs, name, info) } {
        Ok(f) => f,
        Err(e) => return e,
    };
    unsafe { modified(fs, name, info) };
    let id = fs.id;
    match host!(fs, "set allocation", |p| p.set_size(
        id,
        file_id,
        size as u64,
        true
    )) {
        Ok(0) => STATUS_SUCCESS,
        Ok(st) => {
            tracing::debug!(
                status = format!("{st:#x}"),
                "client drive allocation size not taken"
            );
            STATUS_SUCCESS
        }
        Err(e) => e,
    }
}

unsafe fn get_disk_free_space(
    free_available: *mut u64,
    total: *mut u64,
    total_free: *mut u64,
    info: Info,
) -> NTSTATUS {
    let fs = unsafe { fs(info) };
    let (t, f, _) = fs.volume().unwrap_or((0, 0, String::new()));
    unsafe {
        if !free_available.is_null() {
            *free_available = f;
        }
        if !total.is_null() {
            *total = t;
        }
        if !total_free.is_null() {
            *total_free = f;
        }
    }
    STATUS_SUCCESS
}

// The arguments of Dokany's GetVolumeInformation callback.
#[allow(clippy::too_many_arguments)]
unsafe fn get_volume_information(
    name: *mut u16,
    name_size: u32,
    serial: *mut u32,
    max_component: *mut u32,
    flags: *mut u32,
    fs_name: *mut u16,
    fs_name_size: u32,
    info: Info,
) -> NTSTATUS {
    let fs = unsafe { fs(info) };
    let fs_type = fs
        .volume()
        .map(|v| v.2)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "NTFS".into());
    let put = |dst: *mut u16, size: u32, s: &str| {
        if dst.is_null() || size == 0 {
            return;
        }
        let w: Vec<u16> = s.encode_utf16().take(size as usize - 1).collect();
        unsafe {
            std::ptr::copy_nonoverlapping(w.as_ptr(), dst, w.len());
            *dst.add(w.len()) = 0;
        }
    };
    put(name, name_size, &fs.label);
    put(fs_name, fs_name_size, &fs_type);
    unsafe {
        if !serial.is_null() {
            *serial = fs.id;
        }
        if !max_component.is_null() {
            *max_component = 255;
        }
        if !flags.is_null() {
            *flags = FILE_CASE_PRESERVED_NAMES | FILE_UNICODE_ON_DISK;
        }
    }
    STATUS_SUCCESS
}

/// The client drives feature: `Drive1`, served at [`proto::AGENT_PATH`].
pub struct ClientDrives {
    drives: Arc<Drives>,
    host: Arc<HostLink>,
}

impl ClientDrives {
    /// Made on the agent's runtime: Dokany's threads call the host on it.
    pub fn new() -> Self {
        Self {
            drives: Arc::default(),
            host: HostLink::new(),
        }
    }
}

impl crate::feature::Feature for ClientDrives {
    fn channels(&self) -> Vec<&'static str> {
        vec![proto::iface::DRIVE]
    }

    fn serve<'a>(
        &self,
        builder: zbus::connection::Builder<'a>,
    ) -> zbus::Result<zbus::connection::Builder<'a>> {
        Ok(builder.serve_at(
            proto::AGENT_PATH,
            DriveIface {
                drives: self.drives.clone(),
                host: self.host.clone(),
            },
        ))
    }

    fn reset(&self) {
        self.drives.reset();
    }

    fn stop(&self) {
        self.drives.remove_all();
    }

    fn connected(&self, conn: Option<&Connection>) {
        self.host.set(conn.cloned());
    }
}

struct DriveIface {
    drives: Arc<Drives>,
    host: Arc<HostLink>,
}

#[zbus::interface(name = "fr.ameleo.Agent.Drive1")]
impl DriveIface {
    /// Returns the guest drive letter once mounted.
    async fn add_drive(&self, id: u32, name: &str, label: &str) -> proto::Result<String> {
        tracing::info!(id, name, label, "AddDrive");
        // The client session of the call: a reset that comes after it
        // removes the drive, one before it does not.
        let generation = self.drives.generation();
        let label = if label.is_empty() { name } else { label };
        // A volume label: 32 characters at most for Windows.
        let label: String = label.chars().filter(|c| !c.is_control()).take(32).collect();
        let (drives, host) = (self.drives.clone(), self.host.clone());
        let res = tokio::task::spawn_blocking(move || drives.add(&host, generation, id, &label))
            .await
            .map_err(|e| proto::Error::Failed(e.to_string()))?;
        match res {
            Ok(letter) => Ok(format!("{letter}")),
            Err(e) => {
                tracing::warn!(id, "AddDrive failed: {e}");
                Err(proto::Error::Failed(e))
            }
        }
    }

    async fn remove_drive(&self, id: u32) {
        tracing::info!(id, "RemoveDrive");
        let drives = self.drives.clone();
        let _ = tokio::task::spawn_blocking(move || drives.remove(id)).await;
    }

    fn changed(&self, id: u32, changes: Vec<(u32, String)>) {
        tracing::debug!(id, changes = changes.len(), "drive changed on the client");
        self.drives.changed(id, &changes);
    }
}
