//! Unix symlink copy, move, remove, and size tests.

#![cfg(unix)]

mod common;

use common::*;
use dir_tree_ops::{copy_tree, move_tree, remove_tree, tree_size, Options};
use std::fs;
use std::path::PathBuf;

#[test]
fn sl_01_relative_link_preserved_verbatim() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("a"), b"abc");
    symlink("a", &src.join("la"));
    let summary = copy_tree(&src, &dst, &Options::default()).unwrap();
    assert!(fs::symlink_metadata(dst.join("la"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_link(dst.join("la")).unwrap(), PathBuf::from("a"));
    assert_eq!(summary.symlinks, 1);
}

#[test]
fn sl_02_dir_link_not_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("real").join("x"), b"xy");
    symlink("real", &src.join("ld"));
    let summary = copy_tree(&src, &dst, &Options::default()).unwrap();
    assert!(fs::symlink_metadata(dst.join("ld"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        fs::read_link(dst.join("ld")).unwrap(),
        PathBuf::from("real")
    );
    // One real file copied: the walker did not descend through the link.
    assert_eq!(summary.files, 1);
    assert_eq!(summary.bytes_copied, 2);
}

#[test]
fn sl_03_dangling_link_copies_as_dangling_link() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    fs::create_dir(&src).unwrap();
    symlink("missing", &src.join("dead"));
    let summary = copy_tree(&src, &dst, &Options::default()).unwrap();
    assert!(is_dangling_symlink(&dst.join("dead")));
    assert_eq!(
        fs::read_link(dst.join("dead")).unwrap(),
        PathBuf::from("missing")
    );
    assert_eq!(summary.symlinks, 1);
}

#[test]
fn sl_04_absolute_target_preserved_verbatim() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    fs::create_dir(&src).unwrap();
    symlink("/etc/hostname", &src.join("abs"));
    copy_tree(&src, &dst, &Options::default()).unwrap();
    assert_eq!(
        fs::read_link(dst.join("abs")).unwrap(),
        PathBuf::from("/etc/hostname")
    );
}

#[test]
fn sl_05_parent_cycle_terminates() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    fs::create_dir_all(src.join("a")).unwrap();
    symlink("..", &src.join("loop"));
    let summary = copy_tree(&src, &dst, &Options::default()).unwrap();
    assert_eq!(
        fs::read_link(dst.join("loop")).unwrap(),
        PathBuf::from("..")
    );
    assert_eq!(summary.symlinks, 1);
    assert_eq!(summary.files, 0);
}

#[test]
fn sl_06_link_out_of_tree_never_read() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let src = root.join("src");
    let outside = root.join("outside");
    let dst = root.join("dst");
    fs::create_dir_all(&src).unwrap();
    write_file(&outside.join("secret"), b"123456789");
    symlink("../outside", &src.join("ld"));
    let summary = copy_tree(&src, &dst, &Options::default()).unwrap();
    assert!(fs::symlink_metadata(dst.join("ld"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(summary.bytes_copied, 0);
    assert_eq!(summary.files, 0);
}

#[test]
fn sl_07_move_carries_dangling_link_and_clears_src() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    fs::create_dir(&src).unwrap();
    symlink("missing", &src.join("dead"));
    move_tree(&src, &dst, &Options::default()).unwrap();
    assert!(is_dangling_symlink(&dst.join("dead")));
    assert!(!src.exists());
}

#[test]
fn sl_08_remove_deletes_link_as_link() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    write_file(&tree.join("target").join("x"), b"xy");
    symlink("target", &tree.join("ld"));
    remove_tree(&tree).unwrap();
    assert!(!tree.exists());
}

#[test]
fn sl_09_remove_never_follows_out_of_tree_link() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let outside = root.join("outside");
    let tree = root.join("tree");
    write_file(&outside.join("x"), b"xy");
    fs::create_dir(&tree).unwrap();
    symlink("../outside", &tree.join("ld"));
    remove_tree(&tree).unwrap();
    assert!(!tree.exists());
    assert_eq!(read(&outside.join("x")), b"xy");
}

#[test]
fn sl_10_remove_of_link_path_removes_only_the_link() {
    let tmp = tempfile::tempdir().unwrap();
    let somewhere = tmp.path().join("somewhere");
    write_file(&somewhere.join("x"), b"xy");
    let tree = tmp.path().join("tree");
    fs::create_dir(&tree).unwrap();
    let link = tree.join("ld");
    symlink(&somewhere, &link);
    remove_tree(&link).unwrap();
    assert!(fs::symlink_metadata(&link).is_err());
    assert_eq!(read(&somewhere.join("x")), b"xy");
}

#[test]
fn sl_11_tree_size_counts_link_length() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    write_file(&src.join("a"), &vec![0u8; 1000]);
    symlink("a", &src.join("la"));
    assert_eq!(tree_size(&src).unwrap(), 1000 + 1);
}
