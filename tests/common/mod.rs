#![allow(dead_code)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use mmap_wal_log::{demo_payload, Log, Options};

pub fn tmp(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "mwl-it-{}-{}-{}",
        std::process::id(),
        name,
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("log")
}

pub fn small() -> Options {
    Options {
        initial_capacity: 4096,
        ..Options::default()
    }
}

/// Writes `n` demo records and returns (path, offsets of each record, used bytes).
pub fn write_demo(name: &str, n: u64) -> (PathBuf, Vec<u64>, u64) {
    let p = tmp(name);
    let (mut log, _) = Log::open(&p, small()).unwrap();
    for i in 0..n {
        log.append(&demo_payload(i)).unwrap();
    }
    log.flush().unwrap();
    let offs = log.iter().map(|r| r.offset).collect();
    let used = log.used_bytes();
    (p, offs, used)
}

/// Asserts the log holds exactly demo records 0..n.
pub fn assert_demo_prefix(log: &Log, n: u64) {
    assert_eq!(log.next_seq(), n);
    let mut count = 0;
    for rec in log.iter() {
        assert_eq!(rec.seq, count);
        assert_eq!(
            rec.payload,
            demo_payload(rec.seq).as_slice(),
            "seq {}",
            rec.seq
        );
        count += 1;
    }
    assert_eq!(count, n);
}
