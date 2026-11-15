// SPDX-License-Identifier: GPL-3.0-only

//! File lists on the clipboard, MS-RDPECLIP style.
//!
//! Guest to host: a `CF_HDROP` (Explorer copy) becomes a CLIPRDR_FILELIST,
//! the packed `FILEGROUPDESCRIPTORW` that RDP puts on the wire, and file
//! contents are read on demand (`FileClipboard1.FileContents`).
//!
//! Everything that touches the guest files runs as the logged on user: the
//! service is SYSTEM, a path on the clipboard must not give the user (or the
//! host) access to files the user cannot read.

use std::collections::HashMap;
use std::fs::File;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use proto::Error;
use proto::file_contents::{DESCRIPTOR_SIZE, MAX_RANGE, RANGE, SIZE};

use crate::user::Impersonation;

const FD_ATTRIBUTES: u32 = 0x0004;
const FD_WRITESTIME: u32 = 0x0020;
const FD_FILESIZE: u32 = 0x0040;
const FD_SHOWPROGRESSUI: u32 = 0x4000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
/// `cFileName` is `WCHAR[260]`, NUL included.
const MAX_NAME: usize = 259;
/// Guard against copying a whole drive by mistake.
const MAX_ENTRIES: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Name relative to the copied items, `\` separated.
    pub name: String,
    pub attributes: u32,
    pub size: u64,
    /// FILETIME.
    pub write_time: u64,
    /// Absolute path in the guest (empty for host entries).
    pub path: PathBuf,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }
}

/// The paths of a `CF_HDROP`, as the agent reads it from the clipboard: a
/// `DROPFILES` header, then NUL-terminated paths up to an empty one, wide or
/// ANSI as `fWide` says.
pub fn hdrop_paths(raw: &[u8]) -> Result<Vec<PathBuf>, Error> {
    use std::os::windows::ffi::OsStringExt;
    let bad = |what: &str| Error::InvalidArgument(format!("CF_HDROP: {what}"));
    let u32_at = |o: usize| {
        raw.get(o..o + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    };
    let offset = u32_at(0).ok_or_else(|| bad("short header"))? as usize;
    let wide = u32_at(16).ok_or_else(|| bad("short header"))? != 0;
    let list = raw
        .get(offset..)
        .filter(|_| offset >= 20)
        .ok_or_else(|| bad("bad offset"))?;
    let names: Vec<std::ffi::OsString> = if wide {
        let units: Vec<u16> = list
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        units
            .split(|&u| u == 0)
            .take_while(|n| !n.is_empty())
            .map(std::ffi::OsString::from_wide)
            .collect()
    } else {
        // ANSI paths: old apps only. Taken as Latin-1 if not UTF-8.
        list.split(|&b| b == 0)
            .take_while(|n| !n.is_empty())
            .map(|n| String::from_utf8_lossy(n).into_owned().into())
            .collect()
    };
    if names.is_empty() {
        return Err(Error::NoData("empty file list".into()));
    }
    Ok(names.into_iter().map(PathBuf::from).collect())
}

/// Lists the copied items and everything below the copied directories,
/// each directory before its content. Runs as the user.
pub fn walk(roots: &[PathBuf]) -> Result<Vec<Entry>, Error> {
    let _as_user =
        Impersonation::begin().map_err(|e| Error::Failed(format!("no logged on user: {e}")))?;
    let mut out = Vec::new();
    for root in roots {
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| Error::InvalidArgument(format!("cannot copy {}", root.display())))?;
        add(root, name, &mut out)?;
    }
    Ok(out)
}

fn add(path: &Path, name: String, out: &mut Vec<Entry>) -> Result<(), Error> {
    if out.len() >= MAX_ENTRIES {
        return Err(Error::Failed(format!("more than {MAX_ENTRIES} files")));
    }
    if name.encode_utf16().count() > MAX_NAME {
        return Err(Error::Failed(format!(
            "path too long for the clipboard: {name}"
        )));
    }
    // symlink_metadata: a link is copied as what it is, never followed out
    // of the copied tree.
    let md = std::fs::symlink_metadata(path)
        .map_err(|e| Error::Failed(format!("{}: {e}", path.display())))?;
    let entry = Entry {
        name: name.clone(),
        attributes: md.file_attributes(),
        size: if md.is_dir() { 0 } else { md.file_size() },
        write_time: md.last_write_time(),
        path: path.to_path_buf(),
    };
    let dir = md.is_dir() && !md.file_type().is_symlink();
    out.push(entry);
    if dir {
        let mut children: Vec<_> = std::fs::read_dir(path)
            .map_err(|e| Error::Failed(format!("{}: {e}", path.display())))?
            .filter_map(|e| e.ok())
            .collect();
        children.sort_by_key(|e| e.file_name());
        for child in children {
            let child_name = format!("{name}\\{}", child.file_name().to_string_lossy());
            add(&child.path(), child_name, out)?;
        }
    }
    Ok(())
}

/// CLIPRDR_FILELIST / FILEGROUPDESCRIPTORW.
pub fn encode(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + entries.len() * DESCRIPTOR_SIZE);
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        let start = out.len();
        let flags = FD_ATTRIBUTES | FD_WRITESTIME | FD_FILESIZE | FD_SHOWPROGRESSUI;
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&[0; 16 + 8 + 8]); // clsid, sizel, pointl
        out.extend_from_slice(&e.attributes.to_le_bytes());
        out.extend_from_slice(&[0; 16]); // creation, last access
        out.extend_from_slice(&e.write_time.to_le_bytes());
        out.extend_from_slice(&((e.size >> 32) as u32).to_le_bytes());
        out.extend_from_slice(&(e.size as u32).to_le_bytes());
        let mut name = [0u16; MAX_NAME + 1];
        for (d, s) in name
            .iter_mut()
            .zip(std::ffi::OsStr::new(&e.name).encode_wide().take(MAX_NAME))
        {
            *d = s;
        }
        for u in name {
            out.extend_from_slice(&u.to_le_bytes());
        }
        debug_assert_eq!(out.len() - start, DESCRIPTOR_SIZE);
    }
    out
}

/// Parses a CLIPRDR_FILELIST from the host. Names are checked: they become
/// paths when Explorer pastes them.
pub fn decode(data: &[u8]) -> Result<Vec<Entry>, String> {
    let count = data.get(..4).ok_or("short file list")?;
    let count = u32::from_le_bytes(count.try_into().unwrap()) as usize;
    if count > MAX_ENTRIES || data.len() < 4 + count * DESCRIPTOR_SIZE {
        return Err(format!(
            "file list of {count} entries in {} bytes",
            data.len()
        ));
    }
    let mut out = Vec::with_capacity(count);
    for d in data[4..4 + count * DESCRIPTOR_SIZE]
        .as_chunks::<DESCRIPTOR_SIZE>()
        .0
    {
        let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
        let flags = u32_at(0);
        let attributes = if flags & FD_ATTRIBUTES != 0 {
            u32_at(36)
        } else {
            0
        };
        let write_time = if flags & FD_WRITESTIME != 0 {
            u64::from_le_bytes(d[56..64].try_into().unwrap())
        } else {
            0
        };
        let size = if flags & FD_FILESIZE != 0 {
            (u64::from(u32_at(64)) << 32) | u64::from(u32_at(68))
        } else {
            0
        };
        let units: Vec<u16> = d[72..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let name = crate::win::from_wide(&units);
        check_relative(&name)?;
        out.push(Entry {
            name,
            attributes,
            size,
            write_time,
            path: PathBuf::new(),
        });
    }
    Ok(out)
}

/// A host name must stay below the paste target: no drive, no root, no
/// `..`, no reserved characters.
fn check_relative(name: &str) -> Result<(), String> {
    let bad = name.is_empty()
        || name.starts_with(['\\', '/'])
        || name.contains(['/', ':', '*', '?', '"', '<', '>', '|'])
        || name.chars().any(char::is_control)
        || name
            .split('\\')
            .any(|c| c.is_empty() || c == "." || c == "..");
    if bad {
        Err(format!("invalid file name in the host list: {name:?}"))
    } else {
        Ok(())
    }
}

/// The file last read for the host, kept open: a client reads a file
/// in many File Contents Requests, opening it as the user each time (user
/// token, impersonation, open) costs more than the read.
pub type OpenFile = std::sync::Mutex<Option<(PathBuf, File)>>;

/// Attributes that describe the file itself. The others describe how the
/// client stores it (reparse point, sparse, compressed, encrypted, offline,
/// cloud placeholder recall / pinning — a OneDrive "Files On-Demand" file is
/// `REPARSE_POINT | SPARSE | RECALL_ON_DATA_ACCESS`): meaningless in the
/// guest, and Explorer silently gives up on a virtual file carrying them.
const KEPT_ATTRIBUTES: u32 = 0x0001 // READONLY
    | 0x0002 // HIDDEN
    | 0x0004 // SYSTEM
    | 0x0010 // DIRECTORY
    | 0x0020 // ARCHIVE
    | 0x0080 // NORMAL
    | 0x2000; // NOT_CONTENT_INDEXED

/// Keeps only [`KEPT_ATTRIBUTES`] in every descriptor of a
/// CLIPRDR_FILELIST, in place, before Explorer gets it, and sets
/// `FD_SHOWPROGRESSUI`: without it Explorer copies with no progress dialog,
/// so no throughput graph. Returns the descriptors whose attributes changed.
pub fn clean_attributes(blob: &mut [u8]) -> usize {
    let Some(count) = blob
        .get(..4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()) as usize)
    else {
        return 0;
    };
    let mut changed = 0;
    for d in blob[4..]
        .as_chunks_mut::<DESCRIPTOR_SIZE>()
        .0
        .iter_mut()
        .take(count)
    {
        let flags = u32::from_le_bytes(d[0..4].try_into().unwrap()) | FD_SHOWPROGRESSUI;
        d[0..4].copy_from_slice(&flags.to_le_bytes());
        if flags & FD_ATTRIBUTES == 0 {
            continue;
        }
        let attr = u32::from_le_bytes(d[36..40].try_into().unwrap());
        let kept = attr & KEPT_ATTRIBUTES;
        if kept != attr {
            d[36..40].copy_from_slice(&kept.to_le_bytes());
            changed += 1;
        }
    }
    changed
}

/// What a file list looks like, for the log.
pub struct Summary {
    pub dirs: usize,
    /// Entries inside a folder (`a\b`).
    pub nested: usize,
    /// Names listed more than once (case-insensitive, as Windows compares).
    pub duplicates: usize,
    pub sample: Vec<String>,
}

pub fn summary(entries: &[Entry]) -> Summary {
    let mut seen = std::collections::HashSet::new();
    let duplicates = entries
        .iter()
        .filter(|e| !seen.insert(e.name.to_lowercase()))
        .count();
    Summary {
        dirs: entries.iter().filter(|e| e.is_dir()).count(),
        nested: entries.iter().filter(|e| e.name.contains('\\')).count(),
        duplicates,
        sample: entries.iter().take(4).map(|e| e.name.clone()).collect(),
    }
}

/// Answers a File Contents Request on `entries`, as the user.
pub fn contents(
    entries: &[Entry],
    open: &OpenFile,
    lindex: i32,
    flags: u32,
    position: u64,
    requested: u32,
) -> Result<Vec<u8>, Error> {
    let entry = usize::try_from(lindex)
        .ok()
        .and_then(|i| entries.get(i))
        .ok_or_else(|| Error::NotFound(format!("file index {lindex}")))?;
    if entry.is_dir() {
        return Err(Error::InvalidArgument(format!(
            "{} is a directory",
            entry.name
        )));
    }
    let mut open = open.lock().unwrap();
    if open.as_ref().is_none_or(|(p, _)| *p != entry.path) {
        let _as_user =
            Impersonation::begin().map_err(|e| Error::Failed(format!("no logged on user: {e}")))?;
        let file = File::open(&entry.path)
            .map_err(|e| Error::Failed(format!("{}: {e}", entry.path.display())))?;
        *open = Some((entry.path.clone(), file));
    }
    let file = &open.as_ref().unwrap().1;
    if flags & SIZE != 0 {
        let size = file
            .metadata()
            .map_err(|e| Error::Failed(e.to_string()))?
            .len();
        return Ok(size.to_le_bytes().to_vec());
    }
    if flags & RANGE == 0 {
        return Err(Error::InvalidArgument(format!("flags {flags:#x}")));
    }
    let want = requested.min(MAX_RANGE) as usize;
    let mut buf = vec![0u8; want];
    let mut got = 0;
    while got < want {
        match file.seek_read(&mut buf[got..], position + got as u64) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.raw_os_error() == Some(38) => break, // ERROR_HANDLE_EOF
            Err(e) => return Err(Error::Failed(e.to_string())),
        }
    }
    buf.truncate(got);
    Ok(buf)
}

/// Guest file lists the host can read: the current one and the ones frozen
/// by `LockClipData`.
#[derive(Default)]
pub struct GuestFiles {
    /// Guest clipboard serial and its list, built on the first request.
    current: Option<(u32, Arc<Vec<Entry>>)>,
    /// Serial of the guest copy on the clipboard now, list built or not.
    serial: Option<u32>,
    locked: HashMap<u32, Locked>,
}

enum Locked {
    /// Taken before the list of the guest copy `serial` was built: bound
    /// to that copy's list only, never to a later one.
    Waiting {
        serial: Option<u32>,
    },
    List(Arc<Vec<Entry>>),
}

/// More locks than this is a host that forgets to unlock.
const MAX_LOCKS: usize = 64;

impl GuestFiles {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// The list for guest clipboard `serial`, if already built.
    pub fn cached(&self, serial: u32) -> Option<Arc<Vec<Entry>>> {
        self.current
            .as_ref()
            .filter(|(s, _)| *s == serial)
            .map(|(_, e)| e.clone())
    }

    pub fn set_current(&mut self, serial: u32, entries: Arc<Vec<Entry>>) {
        // Locks taken on this copy before its list was built (MS-RDPECLIP:
        // a lock freezes the content of the moment it is taken).
        for lock in self.locked.values_mut() {
            if let Locked::Waiting { serial: s } = lock
                && s.is_none_or(|s| s == serial)
            {
                *lock = Locked::List(entries.clone());
            }
        }
        self.current = Some((serial, entries));
    }

    /// A new guest copy: the old list is not the current content anymore.
    pub fn clipboard_changed(&mut self, serial: u32) {
        self.current = None;
        self.serial = Some(serial);
    }

    pub fn lock(&mut self, id: u32) -> Result<(), Error> {
        if self.locked.len() >= MAX_LOCKS && !self.locked.contains_key(&id) {
            return Err(Error::Failed("too many clipboard locks".into()));
        }
        let lock = match &self.current {
            Some((s, e)) if self.serial.is_none_or(|serial| serial == *s) => {
                Locked::List(e.clone())
            }
            _ => Locked::Waiting {
                serial: self.serial,
            },
        };
        self.locked.insert(id, lock);
        Ok(())
    }

    pub fn unlock(&mut self, id: u32) {
        self.locked.remove(&id);
    }

    pub fn entries(&self, clip_data_id: Option<u32>) -> Result<Arc<Vec<Entry>>, Error> {
        match clip_data_id {
            Some(id) => match self.locked.get(&id) {
                Some(Locked::List(e)) => Ok(e.clone()),
                Some(Locked::Waiting { .. }) => Err(Error::NoData(format!(
                    "file list of lock {id} not requested yet"
                ))),
                None => Err(Error::NotFound(format!("clipboard lock {id}"))),
            },
            None => self
                .current
                .as_ref()
                .map(|(_, e)| e.clone())
                .ok_or_else(|| Error::NoData("no file list requested".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, dir: bool, size: u64) -> Entry {
        Entry {
            name: name.into(),
            attributes: if dir { FILE_ATTRIBUTE_DIRECTORY } else { 0x20 },
            size,
            write_time: 0x01DB_0000_1234_5678,
            path: PathBuf::new(),
        }
    }

    fn dropfiles(wide: bool, body: &[u8]) -> Vec<u8> {
        let mut raw = 20u32.to_le_bytes().to_vec();
        raw.extend_from_slice(&[0; 12]); // pt, fNC
        raw.extend_from_slice(&u32::from(wide).to_le_bytes());
        raw.extend_from_slice(body);
        raw
    }

    #[test]
    fn hdrop_lists() {
        let body: Vec<u8> = "C:\\a b\\é.txt\0D:\\dir\0\0"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(
            hdrop_paths(&dropfiles(true, &body)).unwrap(),
            [PathBuf::from("C:\\a b\\é.txt"), PathBuf::from("D:\\dir")]
        );
        assert_eq!(
            hdrop_paths(&dropfiles(false, b"C:\\x.txt\0\0")).unwrap(),
            [PathBuf::from("C:\\x.txt")]
        );
        assert!(matches!(
            hdrop_paths(&dropfiles(true, &[0, 0])),
            Err(Error::NoData(_))
        ));
        assert!(hdrop_paths(&[1, 2, 3]).is_err());
        let mut far = dropfiles(true, &body);
        far[0] = 0xFF;
        assert!(hdrop_paths(&far).is_err());
    }

    #[test]
    fn descriptor_roundtrip() {
        let list = vec![
            entry("dir", true, 0),
            entry("dir\\é.txt", false, 5_000_000_000),
            entry("b.bin", false, 3),
        ];
        let blob = encode(&list);
        assert_eq!(blob.len(), 4 + 3 * 592);
        assert_eq!(&blob[..4], &3u32.to_le_bytes());
        // nFileSizeHigh / nFileSizeLow of the second descriptor.
        let d = &blob[4 + 592..4 + 2 * 592];
        assert_eq!(u32::from_le_bytes(d[64..68].try_into().unwrap()), 1);
        assert_eq!(
            u32::from_le_bytes(d[68..72].try_into().unwrap()),
            (5_000_000_000u64 & 0xFFFF_FFFF) as u32
        );
        assert_eq!(decode(&blob).unwrap(), list);
    }

    /// A OneDrive "Files On-Demand" folder, as mstsc describes it.
    #[test]
    fn cloud_placeholder_attributes_are_dropped() {
        let mut dir = entry("d", true, 0);
        dir.attributes = 0x0000_0410; // DIRECTORY | REPARSE_POINT
        let mut file = entry("d\\f.txt", false, 90);
        file.attributes = 0x0040_1620; // ARCHIVE | SPARSE | REPARSE | OFFLINE | RECALL_ON_DATA_ACCESS
        let mut hidden = entry("d\\.h", false, 1);
        hidden.attributes = 0x0000_0022; // HIDDEN | ARCHIVE: untouched
        let mut blob = encode(&[dir, file, hidden]);
        assert_eq!(clean_attributes(&mut blob), 2);
        let back = decode(&blob).unwrap();
        assert_eq!(back[0].attributes, 0x10);
        assert!(back[0].is_dir());
        assert_eq!(back[1].attributes, 0x20);
        assert_eq!(back[2].attributes, 0x22);
        assert_eq!((back[1].name.as_str(), back[1].size), ("d\\f.txt", 90));
    }

    #[test]
    fn flattened_folder_shows_duplicates() {
        let tree = [
            entry("d", true, 0),
            entry("d\\README.txt", false, 1),
            entry("d\\sub", true, 0),
            entry("d\\sub\\README.txt", false, 1),
        ];
        let s = summary(&tree);
        assert_eq!((s.dirs, s.nested, s.duplicates), (2, 3, 0));
        // The same folder with the paths stripped.
        let flat = [entry("README.txt", false, 1), entry("readme.TXT", false, 1)];
        let s = summary(&flat);
        assert_eq!((s.dirs, s.nested, s.duplicates), (0, 0, 1));
    }

    #[test]
    fn host_names_stay_relative() {
        for bad in [
            "..\\x",
            "a\\..\\..\\x",
            "\\x",
            "C:\\x",
            "a\\\\b",
            "a/b",
            "a\\.\\b",
        ] {
            assert!(decode(&encode(&[entry(bad, false, 1)])).is_err(), "{bad}");
        }
        assert!(decode(&encode(&[entry("a\\b c\\d.txt", false, 1)])).is_ok());
        assert!(decode(&[1, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn locks_before_the_list() {
        let mut g = GuestFiles::default();
        g.clipboard_changed(1);
        g.lock(7).unwrap();
        assert!(matches!(g.entries(Some(7)), Err(Error::NoData(_))));
        let list = Arc::new(vec![entry("a", false, 1)]);
        g.set_current(1, list.clone());
        assert_eq!(g.entries(Some(7)).unwrap(), list);
        g.clipboard_changed(2);
        g.set_current(2, Arc::new(vec![]));
        // The lock keeps the old content.
        assert_eq!(g.entries(Some(7)).unwrap(), list);
        assert!(g.entries(None).unwrap().is_empty());
        g.unlock(7);
        assert!(matches!(g.entries(Some(7)), Err(Error::NotFound(_))));
    }

    #[test]
    fn waiting_lock_keeps_its_copy() {
        // Locked on copy 1, whose list is never built; copy 2's list must
        // not be served under it.
        let mut g = GuestFiles::default();
        g.clipboard_changed(1);
        g.lock(7).unwrap();
        g.clipboard_changed(2);
        g.lock(8).unwrap();
        let list = Arc::new(vec![entry("b", false, 1)]);
        g.set_current(2, list.clone());
        assert!(matches!(g.entries(Some(7)), Err(Error::NoData(_))));
        assert_eq!(g.entries(Some(8)).unwrap(), list);
    }

    /// Runs as the test user: the user token comes from the process (the
    /// console mode path), as SYSTEM it would come from the session.
    #[test]
    fn walk_and_read() {
        let dir = std::env::temp_dir().join(format!("pro-walk-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("f.txt"), b"hello world").unwrap();
        let out = walk(std::slice::from_ref(&dir)).unwrap();
        let top = dir.file_name().unwrap().to_string_lossy().into_owned();
        let names: Vec<_> = out.iter().map(|e| e.name.clone()).collect();
        assert_eq!(
            names,
            [
                top.clone(),
                format!("{top}\\sub"),
                format!("{top}\\sub\\f.txt")
            ]
        );
        assert_eq!(out[2].size, 11);

        let open = OpenFile::default();
        assert_eq!(
            contents(&out, &open, 2, SIZE, 0, 0).unwrap(),
            11u64.to_le_bytes()
        );
        assert_eq!(contents(&out, &open, 2, RANGE, 6, 100).unwrap(), b"world");
        assert_eq!(contents(&out, &open, 2, RANGE, 0, 5).unwrap(), b"hello");
        assert!(contents(&out, &open, 2, RANGE, 11, 5).unwrap().is_empty());
        assert!(contents(&out, &open, 1, RANGE, 0, 5).is_err(), "directory");
        drop(open);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
