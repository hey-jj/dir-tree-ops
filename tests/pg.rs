//! Progress totals, monotonicity, granularity, and panic propagation tests.

mod common;

use common::*;
use dir_tree_ops::{copy_tree_with_progress, Options};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

#[derive(Debug)]
struct Record {
    bytes_copied: u64,
    total_bytes: u64,
    path: PathBuf,
}

fn assert_monotonic(records: &[Record]) {
    for pair in records.windows(2) {
        assert!(
            pair[0].bytes_copied <= pair[1].bytes_copied,
            "bytes_copied must be non-decreasing: {pair:?}"
        );
    }
}

#[test]
fn pg_01_02_monotonic_bytes_and_stable_totals() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("zero"), b"");
    write_file(&src.join("one"), b"x");
    write_file(&src.join("big"), &vec![9u8; 256 * 1024]);
    let mut records: Vec<Record> = Vec::new();
    let summary = copy_tree_with_progress(&src, &dst, &Options::default(), |p| {
        records.push(Record {
            bytes_copied: p.bytes_copied,
            total_bytes: p.total_bytes,
            path: p.current_path.to_path_buf(),
        });
    })
    .unwrap();
    assert_monotonic(&records);
    let expected_total = 256 * 1024 + 1;
    assert_eq!(summary.bytes_copied, expected_total);
    assert_eq!(records.last().unwrap().bytes_copied, expected_total);
    for name in ["zero", "one", "big"] {
        let p = dst.join(name);
        assert!(
            records.iter().any(|r| r.path == p),
            "at least one call per file, including 0-byte: missing {name}"
        );
    }
    // PG-02: totals identical in every call and equal to the pre-scan sum.
    assert!(records.iter().all(|r| r.total_bytes == expected_total));
}

#[test]
fn pg_03_callback_panic_propagates() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("a"), b"abc");
    let result = catch_unwind(AssertUnwindSafe(|| {
        let _ = copy_tree_with_progress(&src, &dst, &Options::default(), |_| {
            panic!("callback panic")
        });
    }));
    assert!(result.is_err(), "the callback panic must reach the caller");
}
