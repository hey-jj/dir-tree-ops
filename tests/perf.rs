//! Median copy timing against `std::fs::copy` on one 256 MiB file.

use dir_tree_ops::{copy_tree, Options};
use std::fs::{self, File};
use std::io::Write;
use std::time::{Duration, Instant};

const SIZE: usize = 256 * 1024 * 1024;
const RUNS: usize = 5;

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

#[test]
fn copy_perf_vs_std_fs_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    let big = src.join("big");
    {
        let mut f = File::create(&big).unwrap();
        let chunk: Vec<u8> = (0..1u32 << 20)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        for _ in 0..(SIZE >> 20) {
            f.write_all(&chunk).unwrap();
        }
        f.sync_all().unwrap();
    }
    let mut std_samples = Vec::with_capacity(RUNS);
    let mut tree_samples = Vec::with_capacity(RUNS);
    for i in 0..RUNS {
        let out = tmp.path().join(format!("std{i}"));
        let start = Instant::now();
        let n = fs::copy(&big, &out).unwrap();
        std_samples.push(start.elapsed());
        assert_eq!(n as usize, SIZE);
        fs::remove_file(&out).unwrap();

        let out = tmp.path().join(format!("tree{i}"));
        fs::create_dir(&out).unwrap();
        let start = Instant::now();
        let summary = copy_tree(&src, &out, &Options::default()).unwrap();
        tree_samples.push(start.elapsed());
        assert_eq!(summary.bytes_copied as usize, SIZE);
        fs::remove_dir_all(&out).unwrap();
    }
    let std_copy = median(std_samples);
    let tree_copy = median(tree_samples);
    let bound = std_copy.mul_f64(2.0) + Duration::from_micros(100);
    println!(
        "256 MiB: std::fs::copy median {std_copy:?}, copy_tree median {tree_copy:?}, bound {bound:?}"
    );
    if cfg!(debug_assertions) {
        return;
    }
    assert!(
        tree_copy <= bound,
        "copy_tree {tree_copy:?} exceeds 2 * std::fs::copy {std_copy:?} + 100 us"
    );
}
