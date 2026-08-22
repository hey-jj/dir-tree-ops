//! Tree removal and byte-size tests, including dangling links.

mod common;

use common::*;
use dir_tree_ops::{remove_tree, tree_size};
use std::fs;
use std::io;

#[test]
fn rm_01_removes_whole_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path().join("t");
    write_file(&t.join("a"), b"1");
    write_file(&t.join("n").join("b"), b"12");
    fs::create_dir_all(t.join("n").join("deep")).unwrap();
    remove_tree(&t).unwrap();
    assert!(!t.exists());
}

#[test]
fn rm_02_missing_path_is_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path().join("t");
    let err = remove_tree(&t).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert!(msg_contains(&err, &t));
}

#[cfg(unix)]
#[test]
fn rm_03_readonly_dir_blocks_with_named_entry() {
    if is_root() {
        return; // read-only dirs do not block root
    }
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path().join("t");
    write_file(&t.join("ro").join("x"), b"1");
    chmod(&t.join("ro"), 0o500);
    let result = remove_tree(&t);
    chmod(&t.join("ro"), 0o700);
    let err = result.unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(msg_contains(&err, &t.join("ro").join("x")));
}

#[cfg(unix)]
#[test]
fn rm_04_broken_symlink_is_removed() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path().join("t");
    symlink("missing", &t);
    assert!(is_dangling_symlink(&t));
    remove_tree(&t).unwrap();
    assert!(
        fs::symlink_metadata(&t).is_err(),
        "the link itself must be gone"
    );
}

#[test]
fn sz_01_sums_regular_files() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path().join("t");
    write_file(&t.join("a"), &[0u8; 100]);
    write_file(&t.join("n").join("b"), &[0u8; 23]);
    assert_eq!(tree_size(&t).unwrap(), 123);
}

#[test]
fn sz_02_empty_dirs_are_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path().join("t");
    fs::create_dir_all(t.join("empty")).unwrap();
    assert_eq!(tree_size(&t).unwrap(), 0);
}

#[test]
fn sz_03_missing_path_is_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let err = tree_size(tmp.path().join("t")).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

#[cfg(unix)]
#[test]
fn sz_04_self_cycle_counts_link_length_only() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tmp.path().join("t");
    fs::create_dir(&t).unwrap();
    symlink(".", &t.join("loop"));
    assert_eq!(tree_size(&t).unwrap(), 1);
}
