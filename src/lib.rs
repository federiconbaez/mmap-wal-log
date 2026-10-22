//! Append-only log on a memory-mapped file.
//!
//! Every record is framed as a 24-byte header plus payload, padded to 8 bytes:
//!
//! ```text
//! 0..4   magic  "MWL1"
//! 4..8   payload length (u32 LE)
//! 8..16  sequence number (u64 LE), 0, 1, 2, ... with no gaps
//! 16..20 CRC32C over length, sequence and payload
//! 20..24 reserved, must be zero
//! then   payload, then zero padding to the next multiple of 8
//! ```
//!
//! The unused part of the file is zero. On [`Log::open`] the file is scanned:
//! a damaged or partial record at the end (a torn append) is zeroed and the log
//! continues from the last good record. Damage followed by valid records is a
//! different thing (bit rot, a lost write), and by default `open` refuses
//! instead of silently dropping good data; [`salvage`] reads around it.

use std::fs::{File, OpenOptions};
use std::io;
use std::ops::Range;
use std::path::Path;

use memmap2::{Mmap, MmapMut};

/// Record magic, "MWL1" in file order.
pub const MAGIC: u32 = u32::from_le_bytes(*b"MWL1");
/// Size of the record header in bytes.
pub const HEADER: usize = 24;
/// Records start on multiples of this.
pub const ALIGN: usize = 8;
/// Largest accepted payload.
pub const MAX_PAYLOAD: usize = 1 << 30;

const MIN_CAPACITY: u64 = 4096;

/// Bytes a record with `len` payload bytes occupies in the file.
pub fn framed_len(len: usize) -> usize {
    (HEADER + len + ALIGN - 1) & !(ALIGN - 1)
}

fn record_crc(len: u32, seq: u64, payload: &[u8]) -> u32 {
    let c = crc32c::crc32c(&len.to_le_bytes());
    let c = crc32c::crc32c_append(c, &seq.to_le_bytes());
    crc32c::crc32c_append(c, payload)
}

fn rd32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn rd64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

enum Probe {
    /// An all-zero header: nothing was ever written here.
    Empty,
    Valid {
        seq: u64,
        len: usize,
        next: usize,
    },
    Bad,
}

fn probe(buf: &[u8], off: usize) -> Probe {
    if off + HEADER > buf.len() {
        return if buf[off..].iter().all(|&b| b == 0) {
            Probe::Empty
        } else {
            Probe::Bad
        };
    }
    let h = &buf[off..off + HEADER];
    if h.iter().all(|&b| b == 0) {
        return Probe::Empty;
    }
    if rd32(h, 0) != MAGIC || rd32(h, 20) != 0 {
        return Probe::Bad;
    }
    let len = rd32(h, 4) as usize;
    if len > MAX_PAYLOAD || off + framed_len(len) > buf.len() {
        return Probe::Bad;
    }
    // Padding is not covered by the CRC, so it must be exactly zero.
    if buf[off + HEADER + len..off + framed_len(len)]
        .iter()
        .any(|&b| b != 0)
    {
        return Probe::Bad;
    }
    let seq = rd64(h, 8);
    let payload = &buf[off + HEADER..off + HEADER + len];
    if record_crc(len as u32, seq, payload) != rd32(h, 16) {
        return Probe::Bad;
    }
    Probe::Valid {
        seq,
        len,
        next: off + framed_len(len),
    }
}

/// One record as stored in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record<'a> {
    pub seq: u64,
    /// Byte offset of the record header in the file.
    pub offset: u64,
    pub payload: &'a [u8],
}

/// What a scan found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Records that are readable.
    pub records: u64,
    /// End of the last record that a writer may continue after.
    pub valid_bytes: u64,
    /// Non-zero bytes after `valid_bytes` that `open` zeroed (a torn append).
    pub zeroed_tail_bytes: u64,
    /// Byte ranges that held damaged data followed by valid records.
    pub damaged: Vec<Range<u64>>,
    /// Sequence numbers that were lost to damage, as half-open ranges.
    pub missing_seqs: Vec<Range<u64>>,
}

struct Scan {
    /// (seq, offset, len) of every accepted record.
    records: Vec<(u64, usize, usize)>,
    /// End offset of the contiguous prefix of good records.
    prefix_end: usize,
    damaged: Vec<Range<u64>>,
    missing: Vec<Range<u64>>,
}

/// Finds the next valid record at or after `from`, on the 8-byte grid.
fn resync(buf: &[u8], from: usize) -> Option<usize> {
    let mut q = from;
    while q + HEADER <= buf.len() {
        if rd32(buf, q) == MAGIC {
            if let Probe::Valid { .. } = probe(buf, q) {
                return Some(q);
            }
        }
        q += ALIGN;
    }
    None
}

/// Scans `buf`. With `keep_going` false it stops at the first damage, which is
/// all `open` needs; `prefix_end` and whether anything valid follows still say
/// whether the damage is a torn tail or mid-log.
fn scan(buf: &[u8], keep_going: bool) -> Scan {
    let mut s = Scan {
        records: Vec::new(),
        prefix_end: 0,
        damaged: Vec::new(),
        missing: Vec::new(),
    };
    let mut off = 0usize;
    let mut expected = 0u64;
    let mut in_prefix = true;
    while off < buf.len() {
        match probe(buf, off) {
            Probe::Valid { seq, len, next } if seq == expected => {
                s.records.push((seq, off, len));
                expected += 1;
                off = next;
                if in_prefix {
                    s.prefix_end = off;
                }
                continue;
            }
            Probe::Empty if buf[off..].iter().all(|&b| b == 0) => break,
            _ => {}
        }
        // Damage (or a stray record with the wrong sequence) at `off`.
        in_prefix = false;
        let Some(q) = resync(buf, off + ALIGN) else {
            break;
        };
        s.damaged.push(off as u64..q as u64);
        if !keep_going {
            break;
        }
        let Probe::Valid { seq, .. } = probe(buf, q) else {
            unreachable!()
        };
        if seq > expected {
            s.missing.push(expected..seq);
        }
        // A record whose sequence goes backwards cannot be trusted as part of
        // this log; skip it by resyncing past it.
        if seq < expected {
            s.damaged.last_mut().unwrap().end = q as u64 + ALIGN as u64;
            off = q + ALIGN;
            continue;
        }
        expected = seq;
        off = q;
    }
    s
}

/// What `open` does when damaged data is followed by valid records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnDamage {
    /// Refuse to open with `ErrorKind::InvalidData`. Nothing is modified.
    #[default]
    Refuse,
    /// Keep the records before the damage and zero everything after it.
    DiscardRest,
}

/// Options for [`Log::open`].
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Size a new file starts with; it doubles when full.
    pub initial_capacity: u64,
    pub on_damage: OnDamage,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            initial_capacity: 1 << 20,
            on_damage: OnDamage::Refuse,
        }
    }
}

/// A writable log. Not shareable between processes while open.
pub struct Log {
    file: File,
    map: MmapMut,
    end: usize,
    next_seq: u64,
    synced_to: usize,
    size_dirty: bool,
}

impl Log {
    /// Opens or creates the log at `path` and recovers it.
    pub fn open(path: impl AsRef<Path>, opts: Options) -> io::Result<(Log, Report)> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let len = file.metadata()?.len();
        let want = if len == 0 {
            opts.initial_capacity.max(MIN_CAPACITY)
        } else {
            len.next_multiple_of(ALIGN as u64)
        };
        let mut size_dirty = false;
        if want != len {
            file.set_len(want)?;
            size_dirty = true;
        }
        // SAFETY: the file is opened read-write by this process only; the
        // documented contract is that nobody else truncates it while open.
        let mut map = unsafe { MmapMut::map_mut(&file)? };
        let s = scan(&map, false);
        if !s.damaged.is_empty() && opts.on_damage == OnDamage::Refuse {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "damaged data at byte {} is followed by valid records; use salvage() or OnDamage::DiscardRest",
                    s.damaged[0].start
                ),
            ));
        }
        let end = s.prefix_end;
        let mut report = Report {
            records: s.records.len() as u64,
            valid_bytes: end as u64,
            zeroed_tail_bytes: 0,
            damaged: s.damaged,
            missing_seqs: Vec::new(),
        };
        if let Some(last) = map[end..].iter().rposition(|&b| b != 0) {
            let dirty = &mut map[end..end + last + 1];
            report.zeroed_tail_bytes = dirty.iter().filter(|&&b| b != 0).count() as u64;
            dirty.fill(0);
            // The zeros must be durable before new appends: otherwise a crash
            // could resurrect stale records with the sequence numbers we are
            // about to reuse.
            map.flush_range(end, last + 1)?;
        }
        if size_dirty {
            file.sync_all()?;
        }
        let next_seq = report.records;
        Ok((
            Log {
                file,
                map,
                end,
                next_seq,
                synced_to: end,
                size_dirty: false,
            },
            report,
        ))
    }

    /// Appends one record and returns its sequence number. Not durable until
    /// [`Log::flush`].
    pub fn append(&mut self, payload: &[u8]) -> io::Result<u64> {
        if payload.len() > MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "payload larger than MAX_PAYLOAD",
            ));
        }
        let need = framed_len(payload.len());
        if self.end + need > self.map.len() {
            self.grow(self.end + need)?;
        }
        let seq = self.next_seq;
        let at = self.end;
        let len = payload.len() as u32;
        // Payload first, header last: an interrupted append leaves either an
        // all-zero header (never written) or one whose CRC does not match.
        self.map[at + HEADER..at + HEADER + payload.len()].copy_from_slice(payload);
        let h = &mut self.map[at..at + HEADER];
        h[4..8].copy_from_slice(&len.to_le_bytes());
        h[8..16].copy_from_slice(&seq.to_le_bytes());
        h[16..20].copy_from_slice(&record_crc(len, seq, payload).to_le_bytes());
        h[20..24].fill(0);
        h[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        self.end += need;
        self.next_seq += 1;
        Ok(seq)
    }

    fn grow(&mut self, at_least: usize) -> io::Result<()> {
        let mut cap = (self.map.len() as u64).max(MIN_CAPACITY);
        while cap < at_least as u64 {
            cap *= 2;
        }
        self.file.set_len(cap)?;
        self.size_dirty = true;
        // SAFETY: as in `open`; the old mapping is dropped after the new one
        // exists, and dirty pages live in the shared page cache either way.
        self.map = unsafe { MmapMut::map_mut(&self.file)? };
        Ok(())
    }

    /// Makes every appended record durable (msync of the dirty range, plus
    /// fsync if the file grew).
    pub fn flush(&mut self) -> io::Result<()> {
        if self.end > self.synced_to {
            self.map
                .flush_range(self.synced_to, self.end - self.synced_to)?;
            self.synced_to = self.end;
        }
        if self.size_dirty {
            self.file.sync_all()?;
            self.size_dirty = false;
        }
        Ok(())
    }

    /// Sequence number the next append gets; also the record count.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Bytes used by records.
    pub fn used_bytes(&self) -> u64 {
        self.end as u64
    }

    /// Current file size.
    pub fn capacity(&self) -> u64 {
        self.map.len() as u64
    }

    /// Iterates over every record in order.
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            buf: &self.map[..self.end],
            off: 0,
        }
    }
}

/// Iterator over the records of an open [`Log`].
pub struct Iter<'a> {
    buf: &'a [u8],
    off: usize,
}

impl<'a> Iterator for Iter<'a> {
    type Item = Record<'a>;
    fn next(&mut self) -> Option<Record<'a>> {
        if self.off >= self.buf.len() {
            return None;
        }
        let h = &self.buf[self.off..];
        let len = rd32(h, 4) as usize;
        let rec = Record {
            seq: rd64(h, 8),
            offset: self.off as u64,
            payload: &h[HEADER..HEADER + len],
        };
        self.off += framed_len(len);
        Some(rec)
    }
}

/// A record copied out by [`salvage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedRecord {
    pub seq: u64,
    pub offset: u64,
    pub payload: Vec<u8>,
}

/// Reads a log read-only and returns every record that can be verified,
/// skipping damaged regions. Never modifies the file.
pub fn salvage(path: impl AsRef<Path>) -> io::Result<(Vec<OwnedRecord>, Report)> {
    let file = File::open(path)?;
    if file.metadata()?.len() == 0 {
        return Ok((Vec::new(), Report::default()));
    }
    // SAFETY: read-only mapping; same no-truncation contract as `open`.
    let map = unsafe { Mmap::map(&file)? };
    let s = scan(&map, true);
    let records: Vec<OwnedRecord> = s
        .records
        .iter()
        .map(|&(seq, off, len)| OwnedRecord {
            seq,
            offset: off as u64,
            payload: map[off + HEADER..off + HEADER + len].to_vec(),
        })
        .collect();
    let report = Report {
        records: records.len() as u64,
        valid_bytes: s.prefix_end as u64,
        zeroed_tail_bytes: 0,
        damaged: s.damaged,
        missing_seqs: s.missing,
    };
    Ok((records, report))
}

/// Deterministic payload for sequence `seq`, used by the crash tests and the
/// CLI so a reader can check every byte without a side channel.
#[doc(hidden)]
pub fn demo_payload(seq: u64) -> Vec<u8> {
    let len = (seq.wrapping_mul(2654435761) % 301) as usize;
    let mut x = seq ^ 0x9E37_79B9_7F4A_7C15;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    pub(crate) fn tmp(name: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mwl-unit-{}-{}-{}",
            std::process::id(),
            name,
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("log")
    }

    #[test]
    fn framing_is_aligned() {
        assert_eq!(framed_len(0), 24);
        assert_eq!(framed_len(1), 32);
        assert_eq!(framed_len(8), 32);
        assert_eq!(framed_len(9), 40);
        assert_eq!(MAGIC.to_le_bytes(), *b"MWL1");
    }

    #[test]
    fn append_iter_reopen() {
        let p = tmp("reopen");
        {
            let (mut log, r) = Log::open(&p, Options::default()).unwrap();
            assert_eq!(r, Report::default());
            for i in 0..100u64 {
                assert_eq!(log.append(&demo_payload(i)).unwrap(), i);
            }
            log.flush().unwrap();
        }
        let (log, r) = Log::open(&p, Options::default()).unwrap();
        assert_eq!(r.records, 100);
        assert_eq!(r.zeroed_tail_bytes, 0);
        assert_eq!(log.next_seq(), 100);
        for (i, rec) in log.iter().enumerate() {
            assert_eq!(rec.seq, i as u64);
            assert_eq!(rec.payload, demo_payload(i as u64));
            assert_eq!(rec.offset % ALIGN as u64, 0);
        }
        assert_eq!(log.iter().count(), 100);
    }

    #[test]
    fn grows_past_initial_capacity() {
        let p = tmp("grow");
        let opts = Options {
            initial_capacity: 4096,
            ..Options::default()
        };
        let (mut log, _) = Log::open(&p, opts).unwrap();
        assert_eq!(log.capacity(), 4096);
        let big = vec![0xAB; 10_000];
        log.append(b"small").unwrap();
        log.append(&big).unwrap();
        log.append(&[]).unwrap();
        log.flush().unwrap();
        assert_eq!(log.capacity(), 16384);
        drop(log);
        let (log, r) = Log::open(&p, opts).unwrap();
        assert_eq!(r.records, 3);
        let v: Vec<_> = log.iter().map(|r| r.payload.len()).collect();
        assert_eq!(v, [5, 10_000, 0]);
    }

    #[test]
    fn layout_on_disk_matches_the_documented_format() {
        let p = tmp("layout");
        let (mut log, _) = Log::open(
            &p,
            Options {
                initial_capacity: 4096,
                ..Options::default()
            },
        )
        .unwrap();
        log.append(b"abc").unwrap();
        log.append(b"").unwrap();
        log.flush().unwrap();
        let offsets: Vec<u64> = log.iter().map(|r| r.offset).collect();
        assert_eq!(offsets, [0, 32]);
        assert_eq!(log.used_bytes(), 56);
        drop(log);
        let raw = std::fs::read(&p).unwrap();
        assert_eq!(raw.len(), 4096);
        assert_eq!(&raw[0..4], b"MWL1");
        assert_eq!(rd32(&raw, 4), 3);
        assert_eq!(rd64(&raw, 8), 0);
        assert_eq!(rd32(&raw, 16), record_crc(3, 0, b"abc"));
        assert_eq!(&raw[24..27], b"abc");
        assert_eq!(&raw[27..32], &[0; 5]);
        assert_eq!(rd64(&raw, 40), 1);
        assert!(raw[56..].iter().all(|&b| b == 0));
    }

    #[test]
    fn empty_file_salvage() {
        let p = tmp("empty");
        std::fs::write(&p, b"").unwrap();
        let (v, r) = salvage(&p).unwrap();
        assert!(v.is_empty());
        assert_eq!(r, Report::default());
    }
}
