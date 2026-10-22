//! `mwl` — small CLI over the log, also used by the crash tests.
//!
//! mwl append-loop PATH FLUSH_EVERY [INITIAL_CAPACITY]
//!     appends demo records forever; after each flush prints the last durable seq
//! mwl verify PATH        opens (recovering) and checks every payload
//! mwl salvage PATH       read-only; lists what can be recovered around damage
use std::io::Write;
use std::process::ExitCode;

use mmap_wal_log::{demo_payload, salvage, Log, Options};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("append-loop") if args.len() >= 3 => append_loop(&args[1], &args[2], args.get(3)),
        Some("verify") if args.len() == 2 => verify(&args[1]),
        Some("salvage") if args.len() == 2 => do_salvage(&args[1]),
        _ => {
            eprintln!("usage: mwl append-loop PATH FLUSH_EVERY [INITIAL_CAPACITY] | verify PATH | salvage PATH");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn append_loop(path: &str, every: &str, cap: Option<&String>) -> std::io::Result<ExitCode> {
    let every: u64 = every
        .parse()
        .map_err(|_| std::io::Error::other("FLUSH_EVERY must be a number"))?;
    let mut opts = Options::default();
    if let Some(c) = cap {
        opts.initial_capacity = c
            .parse()
            .map_err(|_| std::io::Error::other("bad capacity"))?;
    }
    let (mut log, _) = Log::open(path, opts)?;
    let mut out = std::io::stdout().lock();
    loop {
        let seq = log.append(&demo_payload(log.next_seq()))?;
        if (seq + 1) % every.max(1) == 0 {
            log.flush()?;
            writeln!(out, "{seq}")?;
            out.flush()?;
        }
    }
}

fn verify(path: &str) -> std::io::Result<ExitCode> {
    let (log, report) = Log::open(path, Options::default())?;
    for rec in log.iter() {
        if rec.payload != demo_payload(rec.seq).as_slice() {
            eprintln!("payload mismatch at seq {}", rec.seq);
            return Ok(ExitCode::FAILURE);
        }
    }
    println!(
        "records={} zeroed_tail_bytes={}",
        report.records, report.zeroed_tail_bytes
    );
    Ok(ExitCode::SUCCESS)
}

fn do_salvage(path: &str) -> std::io::Result<ExitCode> {
    let (recs, report) = salvage(path)?;
    println!(
        "records={} damaged={:?} missing_seqs={:?}",
        recs.len(),
        report.damaged,
        report.missing_seqs
    );
    Ok(ExitCode::SUCCESS)
}
