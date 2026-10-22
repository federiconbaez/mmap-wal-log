//! Appends per second for the mmap log, against the same framing written with
//! write(2). Every sample starts from a fresh file in the temp dir.
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use mmap_wal_log::{framed_len, Log, Options};

fn fresh(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("mwl-bench-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

fn mmap_append(iters: u64, payload: &[u8], flush_every: u64) -> Duration {
    let p = fresh("mmap");
    let (mut log, _) = Log::open(&p, Options::default()).unwrap();
    let t = Instant::now();
    for i in 0..iters {
        log.append(payload).unwrap();
        if flush_every != 0 && (i + 1) % flush_every == 0 {
            log.flush().unwrap();
        }
    }
    log.flush().unwrap();
    let d = t.elapsed();
    drop(log);
    std::fs::remove_file(&p).unwrap();
    d
}

/// Same 24-byte header + padding, written with one write(2) per record.
fn write_append(iters: u64, payload: &[u8], flush_every: u64) -> Duration {
    let p = fresh("write");
    let mut f: File = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
        .unwrap();
    let mut buf = vec![0u8; framed_len(payload.len())];
    buf[24..24 + payload.len()].copy_from_slice(payload);
    let t = Instant::now();
    for i in 0..iters {
        buf[8..16].copy_from_slice(&i.to_le_bytes());
        let c = crc32c::crc32c(&buf[4..16]);
        let c = crc32c::crc32c_append(c, payload);
        buf[16..20].copy_from_slice(&c.to_le_bytes());
        f.write_all(&buf).unwrap();
        if flush_every != 0 && (i + 1) % flush_every == 0 {
            f.sync_data().unwrap();
        }
    }
    f.sync_data().unwrap();
    let d = t.elapsed();
    drop(f);
    std::fs::remove_file(&p).unwrap();
    d
}

fn bench(c: &mut Criterion) {
    for &(size, flush_every) in &[(64usize, 0u64), (1024, 0), (64, 64), (1024, 64), (64, 1)] {
        let payload = vec![0x5Au8; size];
        let mut g = c.benchmark_group(format!("append/{size}B/flush_every_{flush_every}"));
        g.throughput(Throughput::Elements(1));
        if flush_every == 1 {
            g.sample_size(10).measurement_time(Duration::from_secs(5));
        } else {
            g.measurement_time(Duration::from_secs(3));
        }
        g.bench_function(BenchmarkId::from_parameter("mmap"), |b| {
            b.iter_custom(|n| mmap_append(n, &payload, flush_every))
        });
        g.bench_function(BenchmarkId::from_parameter("write"), |b| {
            b.iter_custom(|n| write_append(n, &payload, flush_every))
        });
        g.finish();
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
