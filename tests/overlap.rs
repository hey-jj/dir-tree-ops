//! Path, symlink, and hard-link overlap rejection tests.

mod common;

use common::*;
use dir_tree_ops::{
    copy_tree, copy_tree_with_progress, move_tree, move_tree_with_progress, Op, Options, Overwrite,
};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const VARIANTS: [Overwrite; 3] = [Overwrite::Error, Overwrite::Skip, Overwrite::Replace];

fn opts(overwrite: Overwrite) -> Options {
    Options {
        overwrite,
        ..Options::default()
    }
}

#[derive(Clone, Copy, Debug)]
enum Call {
    Copy,
    CopyProgress,
    Move,
    MoveProgress,
}

const CALLS: [Call; 4] = [
    Call::Copy,
    Call::CopyProgress,
    Call::Move,
    Call::MoveProgress,
];

fn run(call: Call, src: &Path, dst: &Path, o: &Options) -> io::Result<dir_tree_ops::Summary> {
    match call {
        Call::Copy => copy_tree(src, dst, o),
        Call::CopyProgress => copy_tree_with_progress(src, dst, o, |_| {}),
        Call::Move => move_tree(src, dst, o),
        Call::MoveProgress => move_tree_with_progress(src, dst, o, |_| {}),
    }
}

/// The call must be rejected as overlap before any mutation: `InvalidInput`
/// with `PathError { op: Scan, second_path: Some(..) }`, and the whole
/// `root` tree byte-identical before and after.
fn assert_rejected_without_mutation(root: &Path, call: Call, src: &Path, dst: &Path, o: &Options) {
    let before = snapshot(root).expect("root exists");
    let err = match run(call, src, dst, o) {
        Ok(summary) => panic!("{call:?} {src:?} -> {dst:?} returned Ok({summary:?})"),
        Err(err) => err,
    };
    assert_eq!(
        err.kind(),
        io::ErrorKind::InvalidInput,
        "{call:?} {src:?} -> {dst:?}: {err}"
    );
    let ctx = path_ctx(&err);
    assert_eq!(ctx.op, Op::Scan);
    assert!(ctx.second_path.is_some(), "both paths must be named");
    let after = snapshot(root).expect("root exists");
    assert_eq!(
        before, after,
        "{call:?} {src:?} -> {dst:?} mutated the tree"
    );
}

#[test]
fn self_overlap_copy_never_deletes_source() {
    for child_is_dir in [false, true] {
        for overwrite in VARIANTS {
            let tmp = tempfile::tempdir().unwrap();
            let a = tmp.path().join("a");
            let src = a.join("b");
            write_file(&src.join("keep"), b"payload");
            if child_is_dir {
                write_file(&src.join("b").join("deep"), b"deeper");
            } else {
                write_file(&src.join("b"), b"same name as src");
            }
            let before = snapshot(&src).unwrap();
            let err = copy_tree(&src, &a, &opts(overwrite)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
            let ctx = path_ctx(&err);
            assert_eq!(ctx.op, Op::Scan);
            assert_eq!(ctx.second_path.as_deref(), Some(src.as_path()));
            assert_eq!(snapshot(&src).unwrap(), before, "src must be untouched");
            assert_eq!(read(&src.join("keep")), b"payload");
        }
    }
}

/// Small deterministic generator, so the property needs no extra dependency.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Build a random tree under `dir`: files, nested dirs, and (Unix) links
/// whose targets point inside, outside, at a parent, or nowhere.
fn gen_tree(rng: &mut Lcg, dir: &Path, depth: u32) {
    fs::create_dir_all(dir).unwrap();
    let entries = rng.below(4);
    for i in 0..entries {
        let name = format!("e{depth}{i}");
        match rng.below(if depth < 3 { 3 } else { 2 }) {
            0 => {
                let len = rng.below(64) as usize;
                let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
                write_file(&dir.join(&name), &bytes);
            }
            1 => {
                #[cfg(unix)]
                {
                    let target = match rng.below(4) {
                        0 => PathBuf::from(".."),
                        1 => PathBuf::from("missing"),
                        2 => PathBuf::from("."),
                        _ => dir.to_path_buf(),
                    };
                    symlink(&target, &dir.join(&name));
                }
                #[cfg(not(unix))]
                write_file(&dir.join(&name), b"link stand-in");
            }
            _ => gen_tree(rng, &dir.join(&name), depth + 1),
        }
    }
}

#[test]
fn no_mutation_on_aliasing_paths() {
    let mut rejected = 0u64;
    for seed in 1..=24u64 {
        let mut rng = Lcg(seed);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let a = root.join("a");
        let ab = a.join("b");
        // The blocker shape is always present: a/b holds a child named b.
        gen_tree(&mut rng, &ab, 1);
        if rng.below(2) == 0 {
            write_file(&ab.join("b"), b"file child named b");
        } else {
            gen_tree(&mut rng, &ab.join("b"), 2);
        }
        gen_tree(&mut rng, &a.join("c"), 1);
        let mut pairs: Vec<(PathBuf, PathBuf)> = vec![
            (ab.clone(), a.clone()),            // src inside dst
            (a.clone(), ab.clone()),            // dst inside src
            (a.clone(), a.clone()),             // equal
            (a.clone(), ab.join("b")),          // dst deeper inside src
            (ab.clone(), ab.join("b")),         // dst is the collision child
            (ab.join("b").clone(), ab.clone()), // src is the collision child
        ];
        #[cfg(unix)]
        {
            let lnk = root.join("lnk");
            symlink(&a, &lnk);
            let lnk_b = root.join("lnk_b");
            symlink(&ab, &lnk_b);
            let other = root.join("other");
            fs::create_dir(&other).unwrap();
            symlink(&ab, &other.join("x"));
            pairs.extend([
                (lnk.clone(), a.clone()),      // equal through a link
                (a.clone(), lnk.clone()),      // equal, link on the dst side
                (ab.clone(), lnk.clone()),     // src inside dst through a link
                (lnk.join("b"), a.clone()),    // src named through a link
                (lnk_b.clone(), a.clone()),    // src link to a/b, dst a
                (a.clone(), lnk_b.clone()),    // dst link to a/b
                (ab.clone(), other.join("x")), // dst resolves to src
                (lnk.clone(), lnk_b.clone()),  // both through links, nested
            ]);
        }
        // A source that is itself a file or a symlink is rejected with
        // InvalidInput by the "source is a directory" rule, before the
        // overlap rule runs. Keep only pairs whose src is a real directory
        // so the overlap rule is what is tested. Links still appear inside
        // the src path and on the dst side.
        pairs.retain(|(src, _)| {
            fs::symlink_metadata(src)
                .map(|m| m.is_dir())
                .unwrap_or(false)
        });
        for (src, dst) in &pairs {
            for call in CALLS {
                for overwrite in VARIANTS {
                    assert_rejected_without_mutation(&root, call, src, dst, &opts(overwrite));
                    rejected += 1;
                }
            }
        }
    }
    assert!(rejected >= 24 * 5 * 4 * 3, "property ran {rejected} cases");
}

#[test]
#[cfg(unix)]
fn hard_linked_destination_member_is_rejected_before_mutation() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    let dst = tmp.path().join("dst");
    write_file(&src.join("a"), b"source-a");
    write_file(&src.join("b"), b"source-b");
    fs::create_dir(&dst).unwrap();
    fs::hard_link(src.join("a"), dst.join("a")).unwrap();
    write_file(&dst.join("keep"), b"destination");
    let before = snapshot(tmp.path()).unwrap();
    let err = copy_tree(&src, &dst, &opts(Overwrite::Replace)).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    let ctx = path_ctx(&err);
    assert_eq!(ctx.op, Op::Scan);
    assert_eq!(snapshot(tmp.path()).unwrap(), before);
    assert_eq!(read(&src.join("a")), b"source-a");
    assert_eq!(read(&dst.join("keep")), b"destination");
}
