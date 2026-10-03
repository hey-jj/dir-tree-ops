//! Failure injection and internal state tests for copy and staged move paths.

use super::*;
use std::fs;

fn write(p: &Path, bytes: &[u8]) {
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(p, bytes).unwrap();
}

#[cfg(unix)]
fn chmod(p: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
}

fn is_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

/// No `.<dst-name>.*` sibling and no `.part` or `.old` entry under `parent`.
fn no_staging_residue(parent: &Path, dst_name: &str) -> bool {
    let prefix = format!(".{dst_name}.");
    fs::read_dir(parent).unwrap().all(|e| {
        let name = e.unwrap().file_name();
        let lossy = name.to_string_lossy().into_owned();
        !(lossy.starts_with(&prefix) || lossy.ends_with(".part") || lossy.ends_with(".old"))
    })
}

fn move_forced_staged<'a>(
    src: &Path,
    dst: &Path,
    opts: &Options,
    file_hook: Option<FileHook<'a>>,
    post_finalize: Option<&'a mut dyn FnMut()>,
) -> io::Result<Summary> {
    move_tree_impl(
        src,
        dst,
        opts,
        None,
        MoveCtl {
            force_staged: true,
            file_hook,
            post_finalize,
            aside_removal: None,
            parent_sync: None,
        },
    )
}

#[test]
fn mv_02_forced_staged_move_matches_fast_path() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write(&src.join("a"), b"abc");
    write(&src.join("n").join("b"), b"xy");
    let summary = move_forced_staged(&src, &dst, &Options::default(), None, None).unwrap();
    assert_eq!(fs::read(dst.join("a")).unwrap(), b"abc");
    assert_eq!(fs::read(dst.join("n").join("b")).unwrap(), b"xy");
    assert!(!src.exists());
    assert_eq!(summary.files, 2);
    assert_eq!(summary.dirs, 2);
    assert_eq!(summary.bytes_copied, 5);
    assert!(no_staging_residue(tmp.path(), "dst"));
}

#[cfg(unix)]
#[test]
fn mv_05_unreadable_source_file_leaves_src_intact() {
    if is_root() {
        return; // 0o000 does not block root
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write(&src.join("a"), b"abc");
    write(&src.join("bad"), b"xyz");
    chmod(&src.join("bad"), 0o000);
    let err = move_forced_staged(&src, &dst, &Options::default(), None, None).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(err
        .to_string()
        .contains(&src.join("bad").display().to_string()));
    assert_eq!(fs::read(src.join("a")).unwrap(), b"abc");
    assert!(src.join("bad").exists());
    assert!(!dst.exists());
    assert!(no_staging_residue(tmp.path(), "dst"));
    chmod(&src.join("bad"), 0o600);
}

#[test]
fn mv_06_injected_failure_mid_staging_preserves_all_source_files() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    for i in 0..100 {
        write(
            &src.join(format!("f{i:03}")),
            format!("payload-{i}").as_bytes(),
        );
    }
    let mut hook = |seen: u64, _p: &Path| {
        if seen == 50 {
            Err(io::Error::new(io::ErrorKind::Other, "injected failure"))
        } else {
            Ok(())
        }
    };
    let err =
        move_forced_staged(&src, &dst, &Options::default(), Some(&mut hook), None).unwrap_err();
    assert!(err.to_string().contains("injected failure"));
    for i in 0..100 {
        let p = src.join(format!("f{i:03}"));
        assert_eq!(
            fs::read(&p).unwrap(),
            format!("payload-{i}").as_bytes(),
            "source file {i} must survive a failed move"
        );
    }
    assert!(!dst.exists());
    assert!(no_staging_residue(tmp.path(), "dst"));
}

#[cfg(unix)]
#[test]
fn mv_09_blocked_source_removal_reports_remove_phase_with_dst_complete() {
    if is_root() {
        return; // read-only parents do not block root
    }
    let tmp = tempfile::tempdir().unwrap();
    let hold = tmp.path().join("hold");
    let src = hold.join("src");
    let dst = tmp.path().join("dst");
    write(&src.join("a"), b"abc");
    write(&src.join("n").join("b"), b"xy");
    let hold_for_hook = hold.clone();
    let mut post_finalize = move || chmod(&hold_for_hook, 0o500);
    let err = move_forced_staged(
        &src,
        &dst,
        &Options::default(),
        None,
        Some(&mut post_finalize),
    )
    .unwrap_err();
    chmod(&hold, 0o700);
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    let ctx: &PathError = err.get_ref().unwrap().downcast_ref().unwrap();
    assert_eq!(ctx.op, Op::RemoveSource);
    assert_eq!(fs::read(dst.join("a")).unwrap(), b"abc");
    assert_eq!(fs::read(dst.join("n").join("b")).unwrap(), b"xy");
}

#[test]
fn toctou_symlink_swapped_destination_fails_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    let target = tmp.path().join("target");
    write(&src.join("a"), b"new");
    write(&target, b"protected");
    fs::create_dir(&dst).unwrap();
    let slot = dst.join("a");
    let target_for_hook = target.clone();
    let mut hook = move |_seen: u64, _source: &Path| {
        #[cfg(unix)]
        make_symlink(&target_for_hook, &slot)?;
        #[cfg(windows)]
        match make_symlink(&target_for_hook, &slot) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                fs::hard_link(&target_for_hook, &slot)?;
            }
            Err(e) => return Err(e),
        }
        Ok(())
    };
    let err = copy_tree_impl(&src, &dst, &Options::default(), None, Some(&mut hook)).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(&target).unwrap(), b"protected");
}

#[test]
fn move_replace_onto_file_dst_injected_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write(&src.join("a"), b"abc");
    write(&dst, b"123456789");
    let opts = Options {
        overwrite: Overwrite::Replace,
        ..Options::default()
    };
    let mut aside_seen: Option<PathBuf> = None;
    let mut aside_hook = |p: &Path| {
        aside_seen = Some(p.to_path_buf());
        Err(io::Error::new(
            io::ErrorKind::Other,
            "injected aside failure",
        ))
    };
    let err = move_tree_impl(
        &src,
        &dst,
        &opts,
        None,
        MoveCtl {
            force_staged: false,
            file_hook: None,
            post_finalize: None,
            aside_removal: Some(&mut aside_hook),
            parent_sync: None,
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("injected aside failure"));
    let ctx: &PathError = err.get_ref().unwrap().downcast_ref().unwrap();
    assert_eq!(ctx.op, Op::RemoveEntry);
    assert!(aside_seen.is_some(), "the hook must run on the aside path");
    assert_eq!(fs::read(src.join("a")).unwrap(), b"abc");
    assert!(fs::symlink_metadata(&dst).unwrap().is_file());
    assert_eq!(fs::read(&dst).unwrap(), b"123456789");
    assert!(no_staging_residue(tmp.path(), "dst"));
}

#[test]
fn pg_04_forced_staged_conflict_progress_stops_before_total() {
    use std::cell::Cell;
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write(&src.join("a"), b"abc");
    write(&src.join("b"), b"xy");
    write(&dst.join("b"), b"123456789");
    let records: std::cell::RefCell<Vec<(u64, u64)>> = std::cell::RefCell::new(Vec::new());
    let counter = Cell::new(0u64);
    let counter_in_last_call = Cell::new(0u64);
    let mut cb = |p: &Progress<'_>| {
        records.borrow_mut().push((p.bytes_copied, p.total_bytes));
        counter.set(counter.get() + 1);
        counter_in_last_call.set(counter.get());
    };
    let err = move_tree_impl(
        &src,
        &dst,
        &Options::default(),
        Some(&mut cb),
        MoveCtl {
            force_staged: true,
            file_hook: None,
            post_finalize: None,
            aside_removal: None,
            parent_sync: None,
        },
    )
    .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    let records = records.into_inner();
    assert!(!records.is_empty(), "at least one recorded call");
    for pair in records.windows(2) {
        assert!(pair[0].0 <= pair[1].0, "non-decreasing: {pair:?}");
    }
    let pre_scan_total = 5;
    assert!(records.iter().all(|r| r.1 == pre_scan_total));
    let last = records.last().unwrap();
    assert!(last.0 < last.1, "last bytes_copied strictly below total");
    assert_eq!(counter.get(), counter_in_last_call.get());
    assert_eq!(counter.get(), records.len() as u64);
    assert_eq!(fs::read(src.join("a")).unwrap(), b"abc");
    assert_eq!(fs::read(src.join("b")).unwrap(), b"xy");
    assert_eq!(fs::read(dst.join("b")).unwrap(), b"123456789");
    assert!(no_staging_residue(tmp.path(), "dst"));
}

#[test]
fn non_progress_copy_accepts_maximum_representable_buffer_hint() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write(&src.join("a"), b"payload");
    let opts = Options {
        buffer_size: isize::MAX as usize,
        ..Options::default()
    };
    let result = std::panic::catch_unwind(|| copy_tree(&src, &dst, &opts));
    let summary = result.expect("plain copy must not panic").unwrap();
    assert_eq!(summary.bytes_copied, 7);
    assert_eq!(fs::read(dst.join("a")).unwrap(), b"payload");
}

#[test]
fn streaming_frame_classifies_entry_when_yielded() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write(&src.join("entry"), b"old file");
    fs::create_dir(&dst).unwrap();
    let mut frame = Frame {
        src: src.clone(),
        dst,
        entries: read_dir(&src).unwrap(),
        perms: fs::symlink_metadata(&src).unwrap().permissions(),
        created: false,
    };
    fs::remove_file(src.join("entry")).unwrap();
    fs::create_dir(src.join("entry")).unwrap();
    let entry = frame.entries.next().unwrap().unwrap();
    assert!(fs::symlink_metadata(entry.path()).unwrap().is_dir());
}

#[test]
fn byte_accounting_saturates_at_u64_max() {
    assert_eq!(add_bytes(u64::MAX - 3, 9), u64::MAX);
    assert_eq!(add_bytes(10, 20), 30);
}

#[test]
fn mid_operation_source_swap_fails_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    let target = tmp.path().join("target");
    write(&src.join("a"), b"source");
    write(&target, b"protected");
    let target_for_hook = target.clone();
    let mut hook = move |_seen: u64, source: &Path| {
        fs::remove_file(source)?;
        #[cfg(unix)]
        make_symlink(&target_for_hook, source)?;
        #[cfg(windows)]
        fs::write(source, b"replacement")?;
        Ok(())
    };
    let err = copy_tree_impl(&src, &dst, &Options::default(), None, Some(&mut hook)).unwrap_err();
    let ctx: &PathError = err.get_ref().unwrap().downcast_ref().unwrap();
    assert_eq!(ctx.op, Op::CopyFile);
    assert_eq!(fs::read(target).unwrap(), b"protected");
    assert!(!dst.join("a").exists());
}

#[test]
fn move_parent_sync_failure_preserves_source_and_destination() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write(&src.join("a"), b"payload");
    let mut sync_hook = |_parent: &Path| {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "injected parent sync failure",
        ))
    };
    let err = move_tree_impl(
        &src,
        &dst,
        &Options::default(),
        None,
        MoveCtl {
            force_staged: true,
            file_hook: None,
            post_finalize: None,
            aside_removal: None,
            parent_sync: Some(&mut sync_hook),
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("injected parent sync failure"));
    assert_eq!(fs::read(src.join("a")).unwrap(), b"payload");
    assert!(!dst.exists());
    assert!(no_staging_residue(tmp.path(), "dst"));
}

#[cfg(unix)]
#[test]
fn move_replace_second_failure_restores_destination_without_residue() {
    if is_root() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write(&src.join("a"), b"new");
    write(&dst, b"original");
    let parent = tmp.path().to_path_buf();
    let mut aside_hook = move |_aside: &Path| {
        chmod(&parent, 0o500);
        Err(io::Error::new(
            io::ErrorKind::Other,
            "injected removal and rollback pressure",
        ))
    };
    let opts = Options {
        overwrite: Overwrite::Replace,
        ..Options::default()
    };
    let err = move_tree_impl(
        &src,
        &dst,
        &opts,
        None,
        MoveCtl {
            force_staged: false,
            file_hook: None,
            post_finalize: None,
            aside_removal: Some(&mut aside_hook),
            parent_sync: None,
        },
    )
    .unwrap_err();
    assert!(err
        .to_string()
        .contains("injected removal and rollback pressure"));
    assert_eq!(fs::read(src.join("a")).unwrap(), b"new");
    assert_eq!(fs::read(&dst).unwrap(), b"original");
    assert!(no_staging_residue(tmp.path(), "dst"));
}

#[cfg(unix)]
#[test]
fn move_replace_accepts_name_max_destination() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("d".repeat(255));
    write(&src.join("a"), b"new");
    write(&dst, b"old");
    let summary = move_tree(
        &src,
        &dst,
        &Options {
            overwrite: Overwrite::Replace,
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(summary.bytes_copied, 3);
    assert_eq!(fs::read(dst.join("a")).unwrap(), b"new");
    assert!(!src.exists());
    assert!(no_staging_residue(tmp.path(), &"d".repeat(255)));
}
