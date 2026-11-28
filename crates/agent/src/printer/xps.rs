// SPDX-License-Identifier: GPL-3.0-only

//! The OpenXPS packages of the print jobs (ZIP, OPC): reading the job
//! PrintTicket, putting another one in its place.
//!
//! Parts may be interleaved: stored as pieces `<part>/[<n>].piece`, the
//! last `<part>/[<n>].last.piece`, one deflate stream across them. No
//! ZIP64: the jobs are at most 1 GiB, and a package that needs it is left
//! as it is.

const JOB_TICKET: &str = "Metadata/Job_PT.xml";

struct Entry {
    name: String,
    flags: u16,
    #[cfg_attr(not(test), allow(dead_code))]
    method: u16,
    time: u16,
    date: u16,
    compressed: usize,
    local_offset: usize,
    /// The entry's central directory record.
    central: std::ops::Range<usize>,
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

/// The central directory.
fn entries(z: &[u8]) -> Result<Vec<Entry>, String> {
    let from = z.len().saturating_sub(65_557);
    let eocd = (from..z.len().saturating_sub(21))
        .rev()
        .find(|&i| u32_at(z, i) == Some(0x0605_4b50))
        .ok_or("no end of central directory")?;
    let count = u16_at(z, eocd + 10).ok_or("truncated")?;
    let start = u32_at(z, eocd + 16).ok_or("truncated")?;
    if count == 0xFFFF || start == 0xFFFF_FFFF {
        return Err("ZIP64 package".into());
    }
    let mut at = start as usize;
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        if u32_at(z, at) != Some(0x0201_4b50) {
            return Err(format!("bad central directory entry at {at}"));
        }
        let field = |o: usize| u16_at(z, at + o).ok_or("truncated");
        let name_len = field(28)? as usize;
        let extra_len = field(30)? as usize;
        let comment_len = field(32)? as usize;
        let name = String::from_utf8_lossy(z.get(at + 46..at + 46 + name_len).ok_or("truncated")?)
            .into_owned();
        let end = at + 46 + name_len + extra_len + comment_len;
        out.push(Entry {
            name,
            flags: field(8)?,
            method: field(10)?,
            time: field(12)?,
            date: field(14)?,
            compressed: u32_at(z, at + 20).ok_or("truncated")? as usize,
            local_offset: u32_at(z, at + 42).ok_or("truncated")? as usize,
            central: at..end,
        });
        at = end;
    }
    Ok(out)
}

/// Where an entry's data starts, after its local header.
fn data_start(z: &[u8], e: &Entry) -> Option<usize> {
    let at = e.local_offset;
    if u32_at(z, at) != Some(0x0403_4b50) {
        return None;
    }
    Some(at + 30 + u16_at(z, at + 26)? as usize + u16_at(z, at + 28)? as usize)
}

/// An entry's local record: header, data and data descriptor.
fn local_record(z: &[u8], e: &Entry) -> Option<std::ops::Range<usize>> {
    let mut end = data_start(z, e)? + e.compressed;
    if e.flags & 0x0008 != 0 {
        // Data descriptor: crc, sizes, maybe its signature first.
        end += if u32_at(z, end) == Some(0x0807_4b50) {
            16
        } else {
            12
        };
    }
    (end <= z.len()).then_some(e.local_offset..end)
}

/// The index of an interleaved piece of `part`.
fn piece_index(name: &str, part: &str) -> Option<u32> {
    let head = name.get(..part.len())?;
    if !head.eq_ignore_ascii_case(part) {
        return None;
    }
    let (n, tail) = name[part.len()..].strip_prefix("/[")?.split_once(']')?;
    matches!(tail.to_ascii_lowercase().as_str(), ".piece" | ".last.piece")
        .then(|| n.parse().ok())?
}

/// Position of `name` in `part`: `Some(0)` for the whole part.
fn of_part(name: &str, part: &str) -> Option<u32> {
    if name.eq_ignore_ascii_case(part) {
        Some(0)
    } else {
        piece_index(name, part)
    }
}

/// A part's data.
#[cfg(test)]
fn part(z: &[u8], entries: &[Entry], name: &str) -> Option<Vec<u8>> {
    let mut pieces: Vec<(u32, &Entry)> = entries
        .iter()
        .filter_map(|e| of_part(&e.name, name).map(|i| (i, e)))
        .collect();
    pieces.sort_by_key(|(i, _)| *i);
    let method = pieces.first()?.1.method;
    let mut data = Vec::new();
    for (_, e) in &pieces {
        let start = data_start(z, e)?;
        data.extend_from_slice(z.get(start..start + e.compressed)?);
    }
    match method {
        0 => Some(data),
        8 => miniz_oxide::inflate::decompress_to_vec(&data)
            .map_err(|err| tracing::warn!(part = name, "inflate: {err:?}"))
            .ok(),
        m => {
            tracing::warn!(part = name, "compression method {m}");
            None
        }
    }
}

/// The job PrintTicket of a package.
#[cfg(test)]
pub fn job_ticket(job: &[u8]) -> Option<String> {
    let entries = entries(job)
        .map_err(|e| tracing::warn!("print job is not an XPS package: {e}"))
        .ok()?;
    let data = part(job, &entries, JOB_TICKET)?;
    Some(String::from_utf8_lossy(&data).into_owned())
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// The package with `ticket` as its job PrintTicket: the other entries are
/// copied as they are, in their order; the ticket is one stored entry where
/// its first piece was.
pub fn with_job_ticket(z: &[u8], ticket: &str) -> Result<Vec<u8>, String> {
    let entries = entries(z)?;
    let first = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| of_part(&e.name, JOB_TICKET).is_some())
        .min_by_key(|(_, e)| e.local_offset)
        .map(|(i, _)| i)
        .ok_or("no job PrintTicket")?;
    let is_ticket = |e: &Entry| of_part(&e.name, JOB_TICKET).is_some();
    let data = ticket.as_bytes();
    let crc = crc32(data);
    let (time, date) = (entries[first].time, entries[first].date);

    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by_key(|&i| entries[i].local_offset);
    let mut out = Vec::with_capacity(z.len() + data.len());
    let mut offsets = vec![0u32; entries.len()];
    for i in order {
        let e = &entries[i];
        if is_ticket(e) && i != first {
            continue;
        }
        offsets[i] = u32::try_from(out.len()).map_err(|_| "package too large")?;
        if i == first {
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            for v in [20u16, 0, 0, time, date] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(JOB_TICKET.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(JOB_TICKET.as_bytes());
            out.extend_from_slice(data);
        } else {
            let record =
                local_record(z, e).ok_or_else(|| format!("{}: bad local record", e.name))?;
            out.extend_from_slice(&z[record]);
        }
    }
    let directory = u32::try_from(out.len()).map_err(|_| "package too large")?;
    let mut count = 0u16;
    for (i, e) in entries.iter().enumerate() {
        if is_ticket(e) && i != first {
            continue;
        }
        if i == first {
            out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            for v in [20u16, 20, 0, 0, time, date] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(JOB_TICKET.len() as u16).to_le_bytes());
            out.extend_from_slice(&[0u8; 12]); // extra, comment, disk, attributes
            out.extend_from_slice(&offsets[i].to_le_bytes());
            out.extend_from_slice(JOB_TICKET.as_bytes());
        } else {
            let mut record = z[e.central.clone()].to_vec();
            record[42..46].copy_from_slice(&offsets[i].to_le_bytes());
            out.extend_from_slice(&record);
        }
        count += 1;
    }
    let size = u32::try_from(out.len()).map_err(|_| "package too large")? - directory;
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&directory.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ZIP of entries: stored, or deflated when `deflate`.
    fn zip(files: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let mut z = Vec::new();
        let mut central = Vec::new();
        for (name, data, deflate) in files {
            let stored = if *deflate {
                miniz_oxide::deflate::compress_to_vec(data, 6)
            } else {
                data.to_vec()
            };
            let method: u16 = if *deflate { 8 } else { 0 };
            let offset = z.len() as u32;
            let header = |sig: u32, central: bool| {
                let mut h = sig.to_le_bytes().to_vec();
                if central {
                    h.extend_from_slice(&20u16.to_le_bytes());
                }
                for v in [20u16, 0, method, 0x1234, 0x5678] {
                    h.extend_from_slice(&v.to_le_bytes());
                }
                h.extend_from_slice(&crc32(data).to_le_bytes());
                h.extend_from_slice(&(stored.len() as u32).to_le_bytes());
                h.extend_from_slice(&(data.len() as u32).to_le_bytes());
                h.extend_from_slice(&(name.len() as u16).to_le_bytes());
                h.extend_from_slice(&0u16.to_le_bytes());
                h
            };
            z.extend(header(0x0403_4b50, false));
            z.extend_from_slice(name.as_bytes());
            z.extend_from_slice(&stored);
            central.extend(header(0x0201_4b50, true));
            central.extend_from_slice(&[0u8; 10]); // comment, disk, attributes
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd = z.len();
        z.extend_from_slice(&central);
        z.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        let n = files.len() as u16;
        z.extend_from_slice(&[0, 0, 0, 0]);
        z.extend_from_slice(&n.to_le_bytes());
        z.extend_from_slice(&n.to_le_bytes());
        z.extend_from_slice(&(central.len() as u32).to_le_bytes());
        z.extend_from_slice(&(cd as u32).to_le_bytes());
        z.extend_from_slice(&[0, 0]);
        z
    }

    #[test]
    fn job_ticket_of_a_package() {
        let z = zip(&[
            ("Documents/1/Pages/1.fpage", b"<FixedPage/>", false),
            ("Metadata/Job_PT.xml", b"<psf:PrintTicket/>", true),
        ]);
        assert_eq!(job_ticket(&z).as_deref(), Some("<psf:PrintTicket/>"));
        assert_eq!(job_ticket(b"not a zip"), None);
    }

    #[test]
    fn job_ticket_in_interleaved_pieces() {
        let z = zip(&[
            ("Metadata/Job_PT.xml/[1].last.piece", b"Ticket/>", false),
            ("[Content_Types].xml", b"<Types/>", false),
            ("Metadata/Job_PT.xml/[0].piece", b"<psf:Print", false),
        ]);
        assert_eq!(job_ticket(&z).as_deref(), Some("<psf:PrintTicket/>"));
    }

    #[test]
    fn piece_names() {
        assert_eq!(
            piece_index("Metadata/Job_PT.xml/[12].piece", JOB_TICKET),
            Some(12)
        );
        assert_eq!(
            piece_index("Metadata/Job_PT.xml/[3].last.piece", JOB_TICKET),
            Some(3)
        );
        assert_eq!(piece_index("Metadata/Job_PT.xml.rels", JOB_TICKET), None);
        assert_eq!(
            piece_index("Metadata/Other_PT.xml/[0].piece", JOB_TICKET),
            None
        );
    }

    #[test]
    fn crc() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn job_ticket_replaced() {
        let z = zip(&[
            ("[Content_Types].xml/[0].piece", b"<Types>", false),
            ("Metadata/Job_PT.xml/[0].piece", b"<psf:Print", false),
            (
                "Documents/1/Pages/1.fpage",
                b"<FixedPage>page</FixedPage>",
                true,
            ),
            ("Metadata/Job_PT.xml/[1].last.piece", b"Ticket/>", false),
            ("[Content_Types].xml/[1].last.piece", b"</Types>", false),
        ]);
        let ticket = r#"<?xml version="1.0" encoding="UTF-8"?><psf:PrintTicket version="1">client</psf:PrintTicket>"#;
        let out = with_job_ticket(&z, ticket).unwrap();
        assert_eq!(job_ticket(&out).as_deref(), Some(ticket));
        let e = entries(&out).unwrap();
        let names: Vec<&str> = e.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "[Content_Types].xml/[0].piece",
                "Metadata/Job_PT.xml",
                "Documents/1/Pages/1.fpage",
                "[Content_Types].xml/[1].last.piece"
            ]
        );
        // The others unchanged, the order of the local records kept.
        assert_eq!(
            part(&out, &e, "Documents/1/Pages/1.fpage").unwrap(),
            b"<FixedPage>page</FixedPage>"
        );
        assert_eq!(
            part(&out, &e, "[Content_Types].xml").unwrap(),
            b"<Types></Types>"
        );
        assert!(e.windows(2).all(|w| w[0].local_offset < w[1].local_offset));
        // Its CRC, checked against the data.
        let t = &e[1];
        let start = data_start(&out, t).unwrap();
        assert_eq!(
            u32_at(&out, t.central.start + 16),
            Some(crc32(&out[start..start + t.compressed]))
        );
        // No ticket: refused, the caller sends the package as it is.
        assert!(with_job_ticket(&zip(&[("a", b"b", false)]), ticket).is_err());
    }
}
