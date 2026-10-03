use dir_tree_ops::{
    copy_tree, copy_tree_with_progress, move_tree, move_tree_with_progress, Options, Overwrite,
    Summary,
};
use std::{fs, io, path::Path};

fn rejects_oversized_buffer(operation: impl Fn(&Path, &Path, &Options) -> io::Result<Summary>) {
    for buffer_size in [usize::MAX, isize::MAX as usize + 1] {
        for overwrite in [Overwrite::Error, Overwrite::Skip, Overwrite::Replace] {
            for existing_destination in [false, true] {
                let tmp = tempfile::tempdir().unwrap();
                let src = tmp.path().join("src");
                let dst = tmp.path().join("dst");
                fs::create_dir(&src).unwrap();
                fs::write(src.join("data"), b"12345678").unwrap();
                if existing_destination {
                    fs::write(&dst, b"keep destination").unwrap();
                }
                let opts = Options {
                    buffer_size,
                    overwrite,
                };

                let err = operation(&src, &dst, &opts).unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
                assert_eq!(err.to_string(), "buffer_size exceeds maximum allocation");
                assert_eq!(fs::read(src.join("data")).unwrap(), b"12345678");
                if existing_destination {
                    assert_eq!(fs::read(&dst).unwrap(), b"keep destination");
                } else {
                    assert!(!dst.exists(), "destination created before validation");
                }
                assert_eq!(
                    fs::read_dir(tmp.path()).unwrap().count(),
                    if existing_destination { 2 } else { 1 },
                    "unexpected staging entry"
                );
            }
        }
    }
}

#[test]
fn copy_buffer_size_max_returns_error_not_panic() {
    rejects_oversized_buffer(|src, dst, opts| {
        let mut callbacks = 0;
        let result = copy_tree_with_progress(src, dst, opts, |_| callbacks += 1);
        assert_eq!(callbacks, 0);
        result
    });
}

#[test]
fn move_buffer_size_max_returns_error_not_panic() {
    rejects_oversized_buffer(|src, dst, opts| {
        let mut callbacks = 0;
        let result = move_tree_with_progress(src, dst, opts, |_| callbacks += 1);
        assert_eq!(callbacks, 0);
        result
    });
}

#[test]
fn copy_without_progress_rejects_oversized_buffer() {
    rejects_oversized_buffer(|src, dst, opts| copy_tree(src, dst, opts));
}

#[test]
fn move_without_progress_rejects_oversized_buffer() {
    rejects_oversized_buffer(|src, dst, opts| move_tree(src, dst, opts));
}

#[test]
fn staged_move_rejects_oversized_buffer_before_creating_staging() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("data"), b"12345678").unwrap();
    fs::create_dir(&dst).unwrap();
    fs::write(dst.join("data"), b"keep destination").unwrap();
    let opts = Options {
        buffer_size: usize::MAX,
        overwrite: Overwrite::Replace,
    };

    let err = move_tree_with_progress(&src, &dst, &opts, |_| {}).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(fs::read(src.join("data")).unwrap(), b"12345678");
    assert_eq!(fs::read(dst.join("data")).unwrap(), b"keep destination");
    assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 2);
}
