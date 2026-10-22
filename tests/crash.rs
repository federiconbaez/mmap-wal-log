//! Kills a writer process with SIGKILL at random points and checks that every
//! record it reported durable is there, byte-exact, with nothing invented.
//!
//! SIGKILL loses the process, not the page cache: this proves the on-disk
//! format and recovery logic under interrupted appends and remaps, not
//! durability across power loss.
mod common;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::*;
use mmap_wal_log::{demo_payload, Log};

fn round(path: &std::path::Path, flush_every: u64, cap: u64, run_ms: u64) -> (u64, u64) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mwl"))
        .args([
            "append-loop",
            path.to_str().unwrap(),
            &flush_every.to_string(),
            &cap.to_string(),
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut last: Option<u64> = None;
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(l) => last = Some(l.trim().parse().unwrap()),
                Err(_) => break,
            }
        }
        last
    });
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(run_ms) {
        std::thread::sleep(Duration::from_millis(1));
    }
    child.kill().unwrap(); // SIGKILL on Unix
    child.wait().unwrap();
    let acked = reader.join().unwrap();

    let (log, report) = Log::open(path, small()).expect("recovery must succeed after SIGKILL");
    let n = log.next_seq();
    if let Some(a) = acked {
        assert!(n > a, "acked seq {a} but only {n} records recovered");
    }
    for rec in log.iter() {
        assert_eq!(
            rec.payload,
            demo_payload(rec.seq).as_slice(),
            "seq {}",
            rec.seq
        );
    }
    (n, report.zeroed_tail_bytes)
}

#[test]
fn sigkill_rounds_keep_every_acked_record() {
    let p = tmp("sigkill");
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut prev = 0;
    let mut torn = 0;
    let rounds: u64 = std::env::var("MWL_CRASH_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);
    for i in 0..rounds {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let flush_every = [1, 7, 64, 1000][(seed % 4) as usize];
        let run_ms = 20 + seed % 120;
        // Tiny initial capacity: the writer remaps many times per round, so
        // kills land during growth as well.
        let (n, zeroed) = round(&p, flush_every, 4096, run_ms);
        assert!(n >= prev, "round {i}: log shrank from {prev} to {n}");
        prev = n;
        if zeroed > 0 {
            torn += 1;
        }
    }
    eprintln!("{rounds} rounds, {prev} records at the end, {torn} rounds recovered a torn tail");
    assert!(prev > 0);
}
