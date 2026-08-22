//! Copy shape, conflict, permission, and special-entry tests.

mod common;

use common::*;
use dir_tree_ops::{copy_tree, copy_tree_with_progress, Options, Overwrite};
use std::fs;
use std::io;

fn opts(overwrite: Overwrite) -> Options {
    Options {
        overwrite,
        ..Options::default()
    }
}

#[test]
fn cp_01_single_file() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("a"), b"abc");
    let summary = copy_tree(&src, &dst, &Options::default()).unwrap();
    assert_eq!(read(&dst.join("a")), b"abc");
    assert_eq!(summary.bytes_copied, 3);
    assert_eq!(summary.files, 1);
}

#[test]
fn cp_02_nested_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("n1").join("n2").join("deep"), b"12345");
    write_file(&src.join("top"), b"1");
    let summary = copy_tree(&src, &dst, &Options::default()).unwrap();
    assert_eq!(read(&dst.join("n1").join("n2").join("deep")), b"12345");
    assert_eq!(read(&dst.join("top")), b"1");
    assert_eq!(summary.files, 2);
    assert_eq!(summary.dirs, 3);
}

#[test]
fn cp_03_empty_dir_preserved() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    fs::create_dir_all(src.join("empty")).unwrap();
    copy_tree(&src, &dst, &Options::default()).unwrap();
    assert!(dst.join("empty").is_dir());
}

#[cfg(unix)]
#[test]
fn cp_04_permission_bits_preserved() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("a"), b"abcd");
    chmod(&src.join("a"), 0o600);
    fs::create_dir(src.join("sub")).unwrap();
    chmod(&src.join("sub"), 0o700);
    copy_tree(&src, &dst, &Options::default()).unwrap();
    assert_eq!(mode_of(&dst.join("a")), 0o600);
    assert_eq!(mode_of(&dst.join("sub")), 0o700);
}

#[cfg(unix)]
#[test]
fn cp_05_readonly_dir_perms_applied_after_children() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("ro").join("x"), b"xy");
    chmod(&src.join("ro"), 0o500);
    let result = copy_tree(&src, &dst, &Options::default());
    let ro_mode = result.as_ref().ok().map(|_| mode_of(&dst.join("ro")));
    let content = result
        .as_ref()
        .ok()
        .map(|_| read(&dst.join("ro").join("x")));
    // restore writability so TempDir cleanup succeeds
    chmod(&src.join("ro"), 0o700);
    if dst.join("ro").exists() {
        chmod(&dst.join("ro"), 0o700);
    }
    result.unwrap();
    assert_eq!(content.unwrap(), b"xy");
    assert_eq!(ro_mode.unwrap(), 0o500);
}

#[test]
fn cp_06_progress_call_count_tracks_chunks() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    let payload = vec![7u8; 10 * 1024 * 1024];
    write_file(&src.join("big"), &payload);
    let opts = Options {
        buffer_size: 4096,
        ..Options::default()
    };
    let mut calls_for_big = 0u64;
    let big_dst = dst.join("big");
    let summary = copy_tree_with_progress(&src, &dst, &opts, |p| {
        if p.current_path == big_dst {
            calls_for_big += 1;
        }
    })
    .unwrap();
    assert_eq!(read(&big_dst), payload);
    assert_eq!(summary.bytes_copied, payload.len() as u64);
    // 2560 chunk calls plus one completion call for the file.
    assert!(calls_for_big >= 2560, "got {calls_for_big} calls");
    assert!(calls_for_big <= 2561, "got {calls_for_big} calls");
}

#[test]
fn cp_07_conflict_errors_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("a"), b"abc");
    write_file(&dst.join("a"), b"123456789");
    let err = copy_tree(&src, &dst, &opts(Overwrite::Error)).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert!(msg_contains(&err, &dst.join("a")));
    assert_eq!(read(&dst.join("a")), b"123456789");
}

#[test]
fn cp_08_conflict_skip() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("a"), b"abc");
    write_file(&dst.join("a"), b"123456789");
    let summary = copy_tree(&src, &dst, &opts(Overwrite::Skip)).unwrap();
    assert_eq!(read(&dst.join("a")), b"123456789");
    assert_eq!(summary.skipped, 1);
}

#[test]
fn cp_09_conflict_replace() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("a"), b"abc");
    write_file(&dst.join("a"), b"123456789");
    copy_tree(&src, &dst, &opts(Overwrite::Replace)).unwrap();
    assert_eq!(read(&dst.join("a")), b"abc");
}

#[test]
fn cp_10_replace_resolves_type_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("a"), b"abc");
    write_file(&dst.join("a").join("inner"), b"old");
    copy_tree(&src, &dst, &opts(Overwrite::Replace)).unwrap();
    assert!(dst.join("a").is_file());
    assert_eq!(read(&dst.join("a")), b"abc");
}

#[test]
fn cp_11_missing_dst_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let out = tmp.path().join("out");
    write_file(&src.join("a"), b"abc");
    let err = copy_tree(&src, out.join("dst"), &Options::default()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert!(msg_contains(&err, &out));
    assert!(!out.exists());
}

#[cfg(unix)]
#[test]
fn cp_12_unreadable_source_file() {
    if is_root() {
        return; // 0o000 does not block root
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("a"), b"abc");
    write_file(&src.join("b"), b"xyz");
    write_file(&src.join("c"), b"pqr");
    chmod(&src.join("b"), 0o000);
    let err = copy_tree(&src, &dst, &Options::default()).unwrap_err();
    chmod(&src.join("b"), 0o600);
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(msg_contains(&err, &src.join("b")));
    assert_eq!(read(&src.join("a")), b"abc");
    assert_eq!(read(&src.join("b")), b"xyz");
    assert_eq!(read(&src.join("c")), b"pqr");
}

#[test]
fn cp_13_missing_src() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    let err = copy_tree(&src, &dst, &Options::default()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert!(msg_contains(&err, &src));
}

#[test]
fn cp_14_file_src_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src, b"abc");
    let err = copy_tree(&src, &dst, &Options::default()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn cp_15_dst_inside_src() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    write_file(&src.join("a"), b"abc");
    let err = copy_tree(&src, src.join("inner"), &Options::default()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert!(!src.join("inner").exists());
    assert_eq!(read(&src.join("a")), b"abc");
    assert_eq!(fs::read_dir(&src).unwrap().count(), 1);
}

#[test]
fn cp_16_dst_equals_src() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    write_file(&src.join("a"), b"abc");
    let err = copy_tree(&src, &src, &Options::default()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(read(&src.join("a")), b"abc");
    assert_eq!(fs::read_dir(&src).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn cp_17_fifo_is_unsupported() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    fs::create_dir(&src).unwrap();
    let status = std::process::Command::new("mkfifo")
        .arg(src.join("fifo"))
        .status()
        .unwrap();
    assert!(status.success());
    let err = copy_tree(&src, &dst, &Options::default()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    assert!(msg_contains(&err, &src.join("fifo")));
}
