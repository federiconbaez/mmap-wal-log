//! Torn tails, bit flips and mid-log damage, done by editing the file directly.
mod common;

use std::io::ErrorKind;

use common::*;
use mmap_wal_log::{demo_payload, salvage, Log, OnDamage, Options};

const N: u64 = 40;

#[test]
fn file_cut_at_every_byte_of_the_last_two_records() {
    let (p, offs, used) = write_demo("cut", N);
    let original = std::fs::read(&p).unwrap();
    let from = offs[N as usize - 2];
    for cut in from..=used {
        std::fs::write(&p, &original[..cut as usize]).unwrap();
        let (mut log, report) = Log::open(&p, small()).unwrap();
        // A record survives when its header and payload are intact; the cut
        // may fall in its zero padding, which the 8-byte rounding restores.
        let expect = (0..N)
            .filter(|&i| offs[i as usize] + 24 + demo_payload(i).len() as u64 <= cut)
            .count() as u64;
        assert_eq!(report.records, expect, "cut at {cut}");
        assert!(report.damaged.is_empty());
        assert_demo_prefix(&log, expect);
        // The log keeps working after recovery and a second open sees the same.
        log.append(&demo_payload(expect)).unwrap();
        log.flush().unwrap();
        drop(log);
        let (log, r2) = Log::open(&p, small()).unwrap();
        assert_eq!(r2.zeroed_tail_bytes, 0);
        assert_demo_prefix(&log, expect + 1);
    }
}

#[test]
fn partial_append_with_garbage_is_zeroed() {
    let (p, offs, used) = write_demo("garbage", N);
    let mut raw = std::fs::read(&p).unwrap();
    let last = offs[N as usize - 1] as usize;
    // Pretend the last append was interrupted: keep its payload, scramble one
    // header byte; also leave junk after the end of the log.
    raw[last + 9] ^= 0xFF;
    for b in &mut raw[used as usize + 16..used as usize + 40] {
        *b = 0xEE;
    }
    std::fs::write(&p, &raw).unwrap();
    let (log, report) = Log::open(&p, small()).unwrap();
    assert_eq!(report.records, N - 1);
    assert!(report.zeroed_tail_bytes > 24);
    assert_demo_prefix(&log, N - 1);
    drop(log);
    let after = std::fs::read(&p).unwrap();
    assert!(
        after[last..].iter().all(|&b| b == 0),
        "tail must be zero on disk after recovery"
    );
}

#[test]
fn header_never_written_payload_was() {
    let (p, offs, _) = write_demo("hdr", N);
    let mut raw = std::fs::read(&p).unwrap();
    let last = offs[N as usize - 1] as usize;
    raw[last..last + 24].fill(0);
    std::fs::write(&p, &raw).unwrap();
    let (log, report) = Log::open(&p, small()).unwrap();
    assert_eq!(report.records, N - 1);
    assert_demo_prefix(&log, N - 1);
}

#[test]
fn mid_log_damage_is_refused_salvaged_or_discarded() {
    let (p, offs, _) = write_demo("mid", N);
    let mut raw = std::fs::read(&p).unwrap();
    let k = 17usize;
    raw[offs[k] as usize + 30] ^= 0x01; // one bit inside record 17
    std::fs::write(&p, &raw).unwrap();

    // Default: refuse, and do not touch the file.
    let err = Log::open(&p, small()).err().expect("must refuse");
    assert_eq!(err.kind(), ErrorKind::InvalidData);
    assert_eq!(std::fs::read(&p).unwrap(), raw);

    // Salvage: everything except record 17.
    let (recs, report) = salvage(&p).unwrap();
    assert_eq!(recs.len() as u64, N - 1);
    assert!(recs
        .iter()
        .all(|r| r.payload == demo_payload(r.seq) && r.seq != k as u64));
    assert_eq!(report.missing_seqs, vec![k as u64..k as u64 + 1]);
    assert_eq!(report.damaged, vec![offs[k]..offs[k + 1]]);
    assert_eq!(report.valid_bytes, offs[k]);

    // DiscardRest: keep 0..17, and the dropped records stay gone after new
    // appends reuse their sequence numbers.
    let opts = Options {
        on_damage: OnDamage::DiscardRest,
        ..small()
    };
    let (mut log, report) = Log::open(&p, opts).unwrap();
    assert_eq!(report.records, k as u64);
    assert_demo_prefix(&log, k as u64);
    for i in k as u64..k as u64 + 3 {
        log.append(&demo_payload(i)).unwrap();
    }
    log.flush().unwrap();
    drop(log);
    let (log, report) = Log::open(&p, small()).unwrap();
    assert!(report.damaged.is_empty());
    assert_demo_prefix(&log, k as u64 + 3);
}

/// Flips every bit of a small log, one at a time. Whatever open and salvage
/// return must be byte-exact demo records: damage may lose records, never
/// invent or alter one.
#[test]
fn every_single_bit_flip_is_detected() {
    let (p, offs, used) = write_demo("flip", 12);
    let original = std::fs::read(&p).unwrap();
    let mut refused = 0;
    let mut truncated = 0;
    for bit in 0..(used as usize * 8) {
        let mut raw = original.clone();
        raw[bit / 8] ^= 1 << (bit % 8);
        std::fs::write(&p, &raw).unwrap();
        let byte = (bit / 8) as u64;
        let rec = offs.iter().rposition(|&o| o <= byte).unwrap() as u64;

        let (recs, _) = salvage(&p).unwrap();
        assert!(
            recs.iter().all(|r| r.payload == demo_payload(r.seq)),
            "bit {bit}"
        );
        assert!(
            recs.iter().all(|r| r.seq != rec || recs.len() == 12),
            "bit {bit}"
        );
        assert!(
            recs.len() >= 11,
            "one flip loses at most one record (bit {bit})"
        );

        match Log::open(&p, small()) {
            Ok((log, report)) => {
                // Only damage in the last record (or its padding) is a torn tail.
                assert!(rec == 11, "bit {bit} in record {rec} must not open");
                assert!(report.records >= 11);
                let n = log.next_seq();
                assert_demo_prefix(&log, n);
                truncated += 1;
            }
            Err(e) => {
                assert_eq!(e.kind(), ErrorKind::InvalidData, "bit {bit}");
                refused += 1;
            }
        }
    }
    assert!(refused > 0 && truncated > 0);
    eprintln!(
        "{} flips: {refused} refused, {truncated} recovered as torn tail",
        used * 8
    );
}

#[test]
fn stale_record_with_wrong_sequence_is_not_accepted() {
    // A record from some other log copied into the tail must not extend this one.
    let (p, offs, used) = write_demo("stale", 5);
    let mut raw = std::fs::read(&p).unwrap();
    let rec0: Vec<u8> = raw[offs[0] as usize..offs[1] as usize].to_vec();
    raw[used as usize..used as usize + rec0.len()].copy_from_slice(&rec0);
    std::fs::write(&p, &raw).unwrap();
    let (log, report) = Log::open(&p, small()).unwrap();
    assert_eq!(report.records, 5);
    assert!(report.zeroed_tail_bytes > 0);
    assert_demo_prefix(&log, 5);
}
