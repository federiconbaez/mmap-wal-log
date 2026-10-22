//! Random appends, flushes, reopens and tail truncations against a Vec model.
mod common;

use common::*;
use mmap_wal_log::{salvage, Log};
use proptest::prelude::*;

#[derive(Debug, Clone)]
enum Op {
    Append(Vec<u8>),
    Flush,
    Reopen,
    /// Flush, close, cut the file `n` bytes before the end of used data.
    CrashCut(u16),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => proptest::collection::vec(any::<u8>(), 0..600).prop_map(Op::Append),
        1 => Just(Op::Flush),
        1 => Just(Op::Reopen),
        1 => any::<u16>().prop_map(Op::CrashCut),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 200, ..ProptestConfig::default() })]

    #[test]
    fn log_matches_model(ops in proptest::collection::vec(op(), 1..60)) {
        let p = tmp("model");
        let (mut log, _) = Log::open(&p, small()).unwrap();
        let mut model: Vec<Vec<u8>> = Vec::new();
        for o in ops {
            match o {
                Op::Append(v) => {
                    prop_assert_eq!(log.append(&v).unwrap(), model.len() as u64);
                    model.push(v);
                }
                Op::Flush => log.flush().unwrap(),
                Op::Reopen => {
                    log.flush().unwrap();
                    drop(log);
                    let (l, r) = Log::open(&p, small()).unwrap();
                    prop_assert_eq!(r.records, model.len() as u64);
                    prop_assert_eq!(r.zeroed_tail_bytes, 0);
                    log = l;
                }
                Op::CrashCut(n) => {
                    log.flush().unwrap();
                    let used = log.used_bytes();
                    let offs: Vec<(u64, usize)> = log.iter().map(|r| (r.offset, r.payload.len())).collect();
                    drop(log);
                    let cut = used.saturating_sub(n as u64 % 700);
                    let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
                    f.set_len(cut).unwrap();
                    drop(f);
                    let keep = offs.iter().filter(|&&(o, len)| o + 24 + len as u64 <= cut).count();
                    model.truncate(keep);
                    let (l, r) = Log::open(&p, small()).unwrap();
                    prop_assert_eq!(r.records, keep as u64);
                    log = l;
                }
            }
            prop_assert_eq!(log.next_seq(), model.len() as u64);
        }
        let got: Vec<Vec<u8>> = log.iter().map(|r| r.payload.to_vec()).collect();
        prop_assert_eq!(&got, &model);
        log.flush().unwrap();
        drop(log);
        let (recs, report) = salvage(&p).unwrap();
        prop_assert!(report.damaged.is_empty());
        let salvaged: Vec<Vec<u8>> = recs.into_iter().map(|r| r.payload).collect();
        prop_assert_eq!(salvaged, model);
        std::fs::remove_dir_all(p.parent().unwrap()).unwrap();
    }
}
