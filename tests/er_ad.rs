//! Error context and adversarial path tests.

mod common;

use common::*;
use dir_tree_ops::{
    copy_tree, move_tree, remove_tree, tree_size, Op, Options, Overwrite, PathError, Progress,
    Summary,
};
use std::error::Error as StdError;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

#[test]
fn er_01_std_io_error_surface() {
    // Compile-time signature check: every operation returns std::io::Result.
    fn assert_signatures() {
        fn ret_copy(src: &Path, dst: &Path, o: &Options) -> io::Result<Summary> {
            copy_tree(src, dst, o)
        }
        fn ret_copy_progress(src: &Path, dst: &Path, o: &Options) -> io::Result<Summary> {
            dir_tree_ops::copy_tree_with_progress(src, dst, o, |_p: &Progress<'_>| {})
        }
        fn ret_move(src: &Path, dst: &Path, o: &Options) -> io::Result<Summary> {
            move_tree(src, dst, o)
        }
        fn ret_remove(p: &Path) -> io::Result<()> {
            remove_tree(p)
        }
        fn ret_size(p: &Path) -> io::Result<u64> {
            tree_size(p)
        }
        let _ = (
            ret_copy as fn(&Path, &Path, &Options) -> _,
            ret_copy_progress as fn(&Path, &Path, &Options) -> _,
            ret_move as fn(&Path, &Path, &Options) -> _,
            ret_remove as fn(&Path) -> _,
            ret_size as fn(&Path) -> _,
        );
    }
    assert_signatures();

    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("missing");
    let err: io::Error = copy_tree(&src, tmp.path().join("dst"), &Options::default()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert!(msg_contains(&err, &src));
    let ctx = path_ctx(&err);
    assert!(
        ctx.source().is_some(),
        "source() must chain to the wrapped OS error"
    );
}

#[test]
fn er_02_message_names_caller_path() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("no-such-source");
    let err = copy_tree(&src, tmp.path().join("dst"), &Options::default()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
    assert!(msg_contains(&err, &src));
}

#[cfg(unix)]
#[test]
fn ad_01_non_utf8_name_copies_intact() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    fs::create_dir(&src).unwrap();
    let name = OsStr::from_bytes(&[0x66, 0x6f, 0x80]);
    if fs::write(src.join(name), b"abc").is_err() {
        // APFS (macOS) rejects non-UTF-8 names with EILSEQ, so the input
        // tree cannot exist there. The scenario runs on Linux CI.
        return;
    }
    let summary = copy_tree(&src, &dst, &Options::default()).unwrap();
    assert_eq!(read(&dst.join(name)), b"abc");
    assert_eq!(summary.files, 1);
}

#[test]
fn ad_02_hostile_names_handled_verbatim() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    let long = "a".repeat(250);
    let names = ["with\nnewline", "--", "-leading", long.as_str()];
    fs::create_dir(&src).unwrap();
    for name in names {
        write_file(&src.join(name), name.as_bytes());
    }
    fs::create_dir(src.join("--dir")).unwrap();
    copy_tree(&src, &dst, &Options::default()).unwrap();
    for name in names {
        assert_eq!(read(&dst.join(name)), name.as_bytes());
    }
    assert!(dst.join("--dir").is_dir());
    remove_tree(&src).unwrap();
    remove_tree(&dst).unwrap();
    assert!(!src.exists());
    assert!(!dst.exists());
}

#[test]
fn ad_03_deep_chain_no_stack_overflow() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    let mut cur = src.clone();
    for _ in 0..4096 {
        cur.push("d");
        if fs::create_dir(&cur).is_err() {
            cur.pop();
            break;
        }
    }
    // Whatever depth the platform allowed, every operation must either
    // complete or return a clean io::Error, without overflowing the stack.
    let dst = tmp.path().join("dst");
    let copy_result = copy_tree(&src, &dst, &Options::default());
    assert!(copy_result.is_ok() || copy_result.is_err());
    let size_result = tree_size(&src);
    assert!(size_result.is_ok() || size_result.is_err());
    remove_tree(&src).unwrap();
    assert!(!src.exists());
    if dst.exists() {
        remove_tree(&dst).unwrap();
    }
}

struct Soak {
    files: Vec<std::path::PathBuf>,
}

fn build_soak_tree(root: &Path) -> Soak {
    let mut files = Vec::with_capacity(10_000);
    for d in 0..100 {
        let dir = root.join(format!("d{d:02}"));
        fs::create_dir_all(&dir).unwrap();
        for f in 0..100 {
            let p = dir.join(format!("f{f:02}"));
            fs::write(&p, b"x").unwrap();
            files.push(p);
        }
    }
    Soak { files }
}

fn run_deleter(
    files: Vec<std::path::PathBuf>,
    stop: &AtomicBool,
    deleted: &Mutex<Vec<std::path::PathBuf>>,
) {
    // Simple LCG so the deletion order is scattered without a rand dep.
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let n = files.len();
    for _ in 0..n {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let idx = (state >> 33) as usize % n;
        if fs::remove_file(&files[idx]).is_ok() {
            deleted.lock().unwrap().push(files[idx].clone());
        }
    }
}

#[test]
fn ad_04_copy_soak_under_concurrent_deletion() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir(&src).unwrap();
    let soak = build_soak_tree(&src);
    let stop = AtomicBool::new(false);
    let deleted = Mutex::new(Vec::new());
    let result = std::thread::scope(|scope| {
        let files = soak.files.clone();
        let stop_ref = &stop;
        let deleted_ref = &deleted;
        scope.spawn(move || run_deleter(files, stop_ref, deleted_ref));
        let r = copy_tree(&src, tmp.path().join("dst"), &Options::default());
        stop.store(true, Ordering::Relaxed);
        r
    });
    // The call may succeed or return io::Error. Any panic fails this test.
    match result {
        Ok(summary) => assert!(summary.files <= 10_000),
        Err(err) => {
            let _ = err.kind();
        }
    }
}

#[test]
fn ad_05_move_soak_never_loses_undeleted_files() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    fs::create_dir(&src).unwrap();
    // A pre-existing dst with Overwrite::Replace forces the staged path.
    // A move that deleted source entries as it copied would lose files on a
    // mid-move failure. This soak pins source preservation instead.
    write_file(&dst.join("marker"), b"keep");
    let soak = build_soak_tree(&src);
    let stop = AtomicBool::new(false);
    let deleted = Mutex::new(Vec::new());
    let opts = Options {
        overwrite: Overwrite::Replace,
        ..Options::default()
    };
    let result = std::thread::scope(|scope| {
        let files = soak.files.clone();
        let stop_ref = &stop;
        let deleted_ref = &deleted;
        scope.spawn(move || run_deleter(files, stop_ref, deleted_ref));
        let r = move_tree(&src, &dst, &opts);
        stop.store(true, Ordering::Relaxed);
        r
    });
    let deleted = deleted.into_inner().unwrap();
    match result {
        Ok(_) => {
            // The move won the race: dst is complete and src is gone.
            assert!(!src.exists());
        }
        Err(err) => {
            let _ = err.kind();
            // Source safety: every file the deleter did not remove survives.
            for f in &soak.files {
                if !deleted.contains(f) {
                    assert!(
                        f.exists(),
                        "move must not delete source files pre-finalize: {f:?}"
                    );
                }
            }
            // The pre-existing destination is untouched on error.
            assert_eq!(read(&dst.join("marker")), b"keep");
        }
    }
}

const SWAP_REAL: &[u8] = b"real payload bytes";
const SWAP_PROTECTED: &[u8] = b"protected payload";
const SWAP_STAND_IN: &[u8] = b"stand-in payload for targets without symlinks";

/// Swap the raced entry to a carrier for the protected payload. The
/// replacement is staged under a private name and renamed over the entry,
/// so the path never disappears. On Unix the carrier is a symlink to the
/// protected file. Windows has no unprivileged symlink, so the stand-in is
/// a different regular file, caught by the same post-copy identity
/// re-check.
#[cfg(unix)]
fn plant_carrier(staged: &Path, entry: &Path, protected: &Path) {
    let _ = fs::remove_file(staged);
    std::os::unix::fs::symlink(protected, staged).unwrap();
    fs::rename(staged, entry).unwrap();
}

#[cfg(windows)]
fn plant_carrier(staged: &Path, entry: &Path, _protected: &Path) {
    fs::write(staged, SWAP_STAND_IN).unwrap();
    fs::rename(staged, entry).unwrap();
}

fn restore_real(staged: &Path, entry: &Path) {
    fs::write(staged, SWAP_REAL).unwrap();
    fs::rename(staged, entry).unwrap();
}

/// One soak shape: a swapper thread flips `src/a` between a regular file
/// and a carrier for the protected payload while `copy_tree` runs in a
/// loop. `extra_entry` adds a second source file so the walk takes the
/// engine path instead of the single-file path. After every run, whatever
/// the result, a regular file at `dst/a` must hold bytes from a source
/// state and never the protected payload, and a failed run must fail
/// closed at the copy or scan step. The loop only records violations.
/// Panicking inside the scope would leave the swapper spinning while the
/// scope joins it.
fn source_swap_soak(extra_entry: bool) {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    let protected = tmp.path().join("protected");
    let staged = tmp.path().join("flip");
    write_file(&src.join("a"), SWAP_REAL);
    if extra_entry {
        write_file(&src.join("b"), b"steady");
    }
    write_file(&protected, SWAP_PROTECTED);
    let stop = AtomicBool::new(false);
    let mut violations: Vec<String> = Vec::new();
    let mut runs = 0u32;
    std::thread::scope(|scope| {
        let stop_ref = &stop;
        let entry = src.join("a");
        let protected_ref = &protected;
        let staged_ref = &staged;
        scope.spawn(move || {
            while !stop_ref.load(Ordering::Relaxed) {
                plant_carrier(staged_ref, &entry, protected_ref);
                restore_real(staged_ref, &entry);
            }
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        for _ in 0..12_000 {
            if std::time::Instant::now() >= deadline {
                break;
            }
            runs += 1;
            let _ = fs::remove_dir_all(&dst);
            let result = copy_tree(&src, &dst, &Options::default());
            let copied = dst.join("a");
            if let Ok(meta) = fs::symlink_metadata(&copied) {
                if meta.is_file() {
                    match fs::read(&copied) {
                        Ok(bytes) if bytes == SWAP_PROTECTED => {
                            violations.push(format!("run {runs}: protected bytes landed at dst/a"))
                        }
                        Ok(bytes) if bytes != SWAP_REAL && bytes != SWAP_STAND_IN => violations
                            .push(format!(
                                "run {runs}: dst/a holds bytes from no source state"
                            )),
                        Ok(_) => {}
                        Err(e) => violations.push(format!("run {runs}: dst/a unreadable: {e}")),
                    }
                }
            }
            if let Err(err) = result {
                // Acceptable failures, all closed with no destination file:
                // the copy step rejecting a swapped entry (no-follow open
                // failure or the post-copy identity re-check), and the scan
                // step surfacing the platform's EINVAL when lstat races the
                // rename, seen on macOS.
                match err.get_ref().and_then(|s| s.downcast_ref::<PathError>()) {
                    Some(ctx) if ctx.op == Op::CopyFile => {}
                    Some(ctx)
                        if ctx.op == Op::Scan && err.kind() == io::ErrorKind::InvalidInput => {}
                    Some(ctx) => violations.push(format!(
                        "run {runs}: unexpected error at {:?}: kind {:?}: {err}",
                        ctx.op,
                        err.kind()
                    )),
                    None => violations.push(format!("run {runs}: error without PathError: {err}")),
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
    });
    assert!(runs > 100, "soak barely ran: {runs} iterations");
    assert!(
        violations.is_empty(),
        "swap soak violations: {violations:#?}"
    );
    assert_eq!(read(&protected), SWAP_PROTECTED);
}

#[test]
fn ad_06_swapped_source_never_lands_foreign_bytes() {
    source_swap_soak(false);
    source_swap_soak(true);
}
