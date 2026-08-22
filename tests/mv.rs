//! Move safety, replacement, rename-first, and special-entry tests.

mod common;

use common::*;
use dir_tree_ops::{move_tree, Op, Options, Overwrite};
use std::fs;
use std::io;

fn opts(overwrite: Overwrite) -> Options {
    Options {
        overwrite,
        ..Options::default()
    }
}

#[test]
fn mv_01_fast_path_rename() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("a"), b"abc");
    write_file(&src.join("n").join("b"), b"xy");
    #[cfg(unix)]
    let src_ino = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(src.join("a")).unwrap().ino()
    };
    let summary = move_tree(&src, &dst, &Options::default()).unwrap();
    assert_eq!(read(&dst.join("a")), b"abc");
    assert_eq!(read(&dst.join("n").join("b")), b"xy");
    assert!(!src.exists());
    assert_eq!(summary.files, 2);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            fs::metadata(dst.join("a")).unwrap().ino(),
            src_ino,
            "fast path must rename, not copy"
        );
    }
}

#[test]
fn mv_03_conflict_leaves_source_fully_intact() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("a"), b"abc");
    write_file(&src.join("b"), b"xy");
    write_file(&dst.join("b"), b"123456789");
    let err = move_tree(&src, &dst, &opts(Overwrite::Error)).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(
        read(&src.join("a")),
        b"abc",
        "the #64 regression: file 1 must survive"
    );
    assert_eq!(read(&src.join("b")), b"xy");
    assert_eq!(read(&dst.join("b")), b"123456789");
    assert!(no_aside_residue(tmp.path(), "dst"));
}

#[cfg(unix)]
#[test]
fn mv_04_readonly_dst_parent() {
    if is_root() {
        return; // read-only parents do not block root
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let roparent = tmp.path().join("roparent");
    write_file(&src.join("a"), b"abc");
    fs::create_dir(&roparent).unwrap();
    chmod(&roparent, 0o500);
    let err = move_tree(&src, roparent.join("dst"), &Options::default()).unwrap_err();
    let residue = fs::read_dir(&roparent).unwrap().count();
    chmod(&roparent, 0o700);
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(read(&src.join("a")), b"abc");
    assert_eq!(residue, 0, "no staging residue under dst parent");
}

#[test]
fn mv_07_dst_inside_src() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    write_file(&src.join("a"), b"abc");
    let err = move_tree(&src, src.join("inner"), &Options::default()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(read(&src.join("a")), b"abc");
    assert!(!src.join("inner").exists());
}

#[test]
fn mv_08_replace_swaps_whole_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("a"), b"abc");
    write_file(&dst.join("a"), b"123456789");
    move_tree(&src, &dst, &opts(Overwrite::Replace)).unwrap();
    assert_eq!(read(&dst.join("a")), b"abc");
    assert!(!src.exists());
}

#[test]
fn mv_10_missing_src() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    let err = move_tree(&src, &dst, &Options::default()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    let ctx = path_ctx(&err);
    assert_eq!(ctx.op, Op::Scan);
    assert!(!dst.exists());
}

#[test]
fn move_replace_onto_file_dst() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("a"), b"abc");
    write_file(&dst, b"123456789");
    let summary = move_tree(&src, &dst, &opts(Overwrite::Replace)).unwrap();
    assert!(dst.is_dir());
    assert_eq!(read(&dst.join("a")), b"abc");
    assert!(fs::symlink_metadata(&src).is_err(), "src must be gone");
    assert!(no_aside_residue(tmp.path(), "dst"));
    assert_eq!(summary.files, 1);
}

#[cfg(unix)]
#[test]
fn move_replace_onto_symlink_dst_leaves_target() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let real = tmp.path().join("real");
    let dst = tmp.path().join("dst");
    write_file(&src.join("a"), b"abc");
    write_file(&real.join("keep"), b"kept");
    symlink(&real, &dst);
    move_tree(&src, &dst, &opts(Overwrite::Replace)).unwrap();
    assert!(!fs::symlink_metadata(&dst).unwrap().file_type().is_symlink());
    assert_eq!(read(&dst.join("a")), b"abc");
    assert_eq!(read(&real.join("keep")), b"kept");
    assert!(fs::symlink_metadata(&src).is_err());
    assert!(no_aside_residue(tmp.path(), "dst"));
}

#[cfg(unix)]
#[test]
fn move_rename_first_no_prescan() {
    use std::os::unix::fs::MetadataExt;
    if is_root() {
        return; // mode 000 does not block root
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("a"), b"abc");
    write_file(&src.join("locked").join("inner"), b"hidden");
    chmod(&src.join("locked"), 0o000);
    let src_ino = fs::metadata(&src).unwrap().ino();
    let result = move_tree(&src, &dst, &Options::default());
    let moved_locked = dst.join("locked");
    if moved_locked.exists() {
        chmod(&moved_locked, 0o700);
    }
    if src.join("locked").exists() {
        chmod(&src.join("locked"), 0o700);
    }
    let summary = result.unwrap();
    assert_eq!(
        fs::metadata(&dst).unwrap().ino(),
        src_ino,
        "renamed, not copied"
    );
    assert_eq!(read(&dst.join("a")), b"abc");
    assert_eq!(read(&moved_locked.join("inner")), b"hidden");
    assert!(!src.exists());
    // The summary comes from a lenient scan: the unreadable directory
    // counted as one directory, its contents left out.
    assert_eq!(summary.files, 1);
    assert_eq!(summary.dirs, 2);
}

#[cfg(unix)]
#[test]
fn move_special_nodes_by_rename() {
    use std::os::unix::fs::FileTypeExt;
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    fs::create_dir(&src).unwrap();
    let status = std::process::Command::new("mkfifo")
        .arg(src.join("fifo"))
        .status()
        .unwrap();
    assert!(status.success());
    move_tree(&src, &dst, &Options::default()).unwrap();
    assert!(fs::symlink_metadata(dst.join("fifo"))
        .unwrap()
        .file_type()
        .is_fifo());
    assert!(!src.exists());
}
