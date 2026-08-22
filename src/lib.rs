//! Tree operations on directories: copy, move, remove, and size.
//!
//! Four operations over `AsRef<Path>` roots, plus progress-reporting variants
//! for copy and move:
//!
//! - [`copy_tree`]: recursive copy with a streaming, iterative walk and no
//!   whole-tree snapshot. Regular files go through `std::fs::copy`, which
//!   reaches `clonefile` on APFS and `copy_file_range` on Linux.
//! - [`move_tree`]: rename first, staged copy fallback when the rename
//!   cannot apply. The source stays unchanged until the destination is
//!   complete and durable.
//! - [`remove_tree`]: recursive remove that treats symlinks as leaves.
//! - [`tree_size`]: byte lengths of regular files plus symlink target
//!   lengths. Directories contribute zero, and links are leaves.
//!
//! # Destination semantics
//!
//! `dst` names the resulting root: `copy_tree("a/src", "b/dest", ..)` makes
//! `b/dest` a replica of `a/src`. There is no name-appending, so a first and
//! a second run produce the same shape. The mapping is `src/<rel>` to
//! `dst/<rel>`, always, matching `rsync -a src/ dst/`. The parent of `dst`
//! must already exist, and a missing parent is `NotFound` naming the parent.
//! If `dst` exists as a directory, a copy merges into it with per-entry
//! conflict handling from [`Options::overwrite`].
//!
//! # Overlap
//!
//! Every operation resolves both roots before touching anything. `dst` equal
//! to `src`, `dst` inside `src`, and `src` inside `dst` are all rejected with
//! `InvalidInput` carrying a [`PathError`] with `op` [`Op::Scan`] and both
//! paths. Resolution canonicalizes the existing portion of each path, and on
//! Unix also compares `(dev, ino)` along each path, so a symlink alias or a
//! bind mount of one root counts as the same tree. Inside the walk, an
//! existing destination entry is removed only after its identity is checked
//! against every source entry, and the source is removed only by the last
//! step of a move.
//!
//! # Symlinks
//!
//! A symlink copies as a symlink. Every entry is classified through
//! `symlink_metadata`, the target string is reproduced verbatim (relative
//! stays relative, absolute stays absolute), and the walker treats each link
//! as a leaf. A dangling link copies as the same dangling link and is
//! counted in [`Summary::symlinks`]. [`remove_tree`] deletes a link as a
//! link, so the tree behind it survives.
//!
//! # Move safety
//!
//! [`move_tree`] runs in ordered phases: a rename attempt before any read of
//! the source, then (only on a cross-device failure or when replacing an
//! existing destination) a staged copy into a hidden sibling of the final
//! destination with files fsynced and directories fsynced on Unix, a
//! finalize rename, and only then removal of the source. A failure in any
//! phase before source removal returns the error with the source unchanged,
//! the destination as it was, and no hidden sibling left behind. A failure
//! during source removal reports [`Op::RemoveSource`]. The destination is
//! already complete at that point, so no bytes are lost and the call can be
//! retried.
//!
//! # Errors
//!
//! Every operation returns `std::io::Result`. The `io::ErrorKind` of the
//! underlying failure is preserved, and the error payload is a [`PathError`]
//! carrying the operation step and the path (or paths) involved. Downcast via
//! `err.get_ref()` to read them.
//!
//! # TOCTOU stance
//!
//! There are no `exists()` pre-flight checks: existence is learned from each
//! operation's own syscall result. Progress copies claim a destination file
//! with `create_new`. Non-progress copies check the entry just before
//! `std::fs::copy`, which is required for clone-capable filesystem copying.
//! Source files open with `O_NOFOLLOW` on Linux, Android, and
//! the BSD family including macOS. On other Unix targets the open is
//! followed by an `fstat` identity check against the `symlink_metadata`
//! taken just before, and on Windows the handle is opened on the reparse
//! point itself and rejected if it is a link. Each of those fails closed
//! when an entry is swapped for a link between classification and open. The
//! fast copy path verifies that the no-follow handle has the identity seen
//! by the directory scan. `std::fs::copy` then reads the source through its
//! own path-following open, so after it returns the path is re-opened
//! no-follow and compared against that handle. On a mismatch, or when the
//! re-open fails, the new destination file is removed and the call fails
//! with `InvalidInput`. A swap reverted to the original file before the
//! re-check is not detected. Full elimination needs `openat`-style
//! traversal, which std does not portably expose. Residual and documented: a
//! caller that lets another process replace whole directories on the
//! traversal path can still redirect descent. Trees under shared writable
//! parents need stronger isolation than this API provides.
//!
//! # Non-goals (v1)
//!
//! mtime, ownership, and xattr preservation, a follow-links mode,
//! cancellation from the progress callback, single-file helpers, and async.
//! FIFOs, sockets, and device nodes are carried by the rename fast path of a
//! move and report `io::ErrorKind::Unsupported` naming the path when they
//! have to be copied.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::collections::HashSet;
use std::error::Error as StdError;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
mod unit_tests;

/// Options for [`copy_tree`], [`move_tree`], and their progress variants.
///
/// Construct with `Options::default()` and adjust fields as needed.
#[derive(Clone, Debug)]
pub struct Options {
    /// Per-entry destination conflict policy. Default: [`Overwrite::Error`].
    pub overwrite: Overwrite,
    /// Copy buffer size hint for the progress variants, in bytes.
    /// Default: `64 * 1024`. A value of zero is treated as one.
    pub buffer_size: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            overwrite: Overwrite::Error,
            buffer_size: 64 * 1024,
        }
    }
}

/// What to do when a destination entry already exists.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Overwrite {
    /// An existing destination entry stops the operation with
    /// `io::ErrorKind::AlreadyExists`.
    #[default]
    Error,
    /// An existing destination entry is left untouched and counted in
    /// [`Summary::skipped`].
    Skip,
    /// An existing destination entry is replaced. Type conflicts resolve by
    /// removing the destination entry first: a directory tree is removed to
    /// make room for a file, and a symlink is removed as a link, leaving its
    /// target untouched.
    Replace,
}

/// Counts returned by a completed copy or move.
///
/// A move that completed through the rename fast path reports counts taken
/// from a scan of the moved tree at the destination. A directory that scan
/// cannot read counts as one directory and its contents are left out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Total bytes written to destination regular files.
    pub bytes_copied: u64,
    /// Regular files copied.
    pub files: u64,
    /// Directories created.
    pub dirs: u64,
    /// Symlinks reproduced.
    pub symlinks: u64,
    /// Entries left untouched under [`Overwrite::Skip`].
    pub skipped: u64,
}

/// A progress snapshot passed to the callback of
/// [`copy_tree_with_progress`] and [`move_tree_with_progress`].
#[derive(Debug)]
pub struct Progress<'a> {
    /// Bytes copied so far. Non-decreasing across every call of one
    /// operation, and equal to the returned summary's byte count on success.
    pub bytes_copied: u64,
    /// Total regular-file bytes found by the pre-scan. Fixed for the whole
    /// operation. A tree mutated concurrently can make `bytes_copied` land
    /// under or over this value. Monotonicity still holds.
    pub total_bytes: u64,
    /// Entries (files, directories, symlinks) completed so far.
    pub entries_done: u64,
    /// Entries found by the pre-scan. Fixed for the whole operation.
    pub entries_total: u64,
    /// The entry being processed when this snapshot was taken. The first
    /// snapshot of an operation names the destination root.
    pub current_path: &'a Path,
}

/// The operation step a [`PathError`] happened in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Reading directory entries or entry metadata, and the overlap check.
    Scan,
    /// Copying one regular file.
    CopyFile,
    /// Creating a directory or applying its permissions.
    CreateDir,
    /// Reproducing a symlink.
    Symlink,
    /// The rename fast path of a move.
    Rename,
    /// Removing the source tree after a move's destination is complete.
    RemoveSource,
    /// Removing an entry (during [`remove_tree`] or conflict replacement).
    RemoveEntry,
    /// Renaming staging into place at the end of a staged move.
    Finalize,
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Op::Scan => "scan",
            Op::CopyFile => "copy file",
            Op::CreateDir => "create dir",
            Op::Symlink => "symlink",
            Op::Rename => "rename",
            Op::RemoveSource => "remove source",
            Op::RemoveEntry => "remove entry",
            Op::Finalize => "finalize",
        };
        f.write_str(s)
    }
}

/// Downcast target for structured error context.
///
/// Every error returned by this crate is an `io::Error` whose payload is a
/// `PathError`. `Display` renders `"<op> <path>[ -> <second>]: <source
/// error>"`, and `Error::source()` yields the underlying `io::Error`.
///
/// ```
/// use dir_tree_ops::{copy_tree, Options, PathError};
/// let err = copy_tree("/no/such/tree", "/tmp/out", &Options::default()).unwrap_err();
/// let ctx: &PathError = err.get_ref().unwrap().downcast_ref().unwrap();
/// assert!(ctx.path.ends_with("tree"));
/// ```
#[derive(Debug)]
pub struct PathError {
    /// The step that failed.
    pub op: Op,
    /// The path the step was acting on.
    pub path: PathBuf,
    /// The other path involved, for steps that take two.
    pub second_path: Option<PathBuf>,
    source: io::Error,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.op, self.path.display())?;
        if let Some(second) = &self.second_path {
            write!(f, " -> {}", second.display())?;
        }
        write!(f, ": {}", self.source)
    }
}

impl StdError for PathError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(&self.source)
    }
}

fn wrap(op: Op, path: &Path, second: Option<PathBuf>, source: io::Error) -> io::Error {
    let kind = source.kind();
    io::Error::new(
        kind,
        PathError {
            op,
            path: path.to_path_buf(),
            second_path: second,
            source,
        },
    )
}

fn contract(
    op: Op,
    path: &Path,
    second: Option<PathBuf>,
    kind: io::ErrorKind,
    msg: &str,
) -> io::Error {
    wrap(op, path, second, io::Error::new(kind, msg.to_string()))
}

/// Raw OS code for a cross-device rename failure (`EXDEV` /
/// `ERROR_NOT_SAME_DEVICE`), matched directly because
/// `io::ErrorKind::CrossesDevices` stabilized after the MSRV.
#[cfg(unix)]
const CROSS_DEVICE_RAW: i32 = 18;
#[cfg(windows)]
const CROSS_DEVICE_RAW: i32 = 17;

/// `O_NOFOLLOW` where its value is known. `None` selects the identity-check
/// fallback in [`open_source`], so every target has an explicit mechanism.
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    not(any(
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "sparc",
        target_arch = "sparc64"
    ))
))]
const O_NOFOLLOW_FLAG: Option<i32> = Some(0o400000);
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    any(
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "sparc",
        target_arch = "sparc64"
    )
))]
const O_NOFOLLOW_FLAG: Option<i32> = Some(0x20000);
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
))]
const O_NOFOLLOW_FLAG: Option<i32> = Some(0x0100);
#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "tvos",
        target_os = "watchos",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
    ))
))]
const O_NOFOLLOW_FLAG: Option<i32> = None;

#[cfg(unix)]
fn file_id(meta: &Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(unix)]
fn portable_file_id(meta: &Metadata) -> Option<(u64, u64)> {
    Some(file_id(meta))
}

#[cfg(windows)]
fn portable_file_id(_meta: &Metadata) -> Option<(u64, u64)> {
    None
}

#[cfg(unix)]
fn same_file_identity(expected: &Metadata, actual: &Metadata) -> bool {
    portable_file_id(expected) == portable_file_id(actual)
}

#[cfg(windows)]
fn same_file_identity(expected: &Metadata, actual: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    expected.file_attributes() == actual.file_attributes()
        && expected.creation_time() == actual.creation_time()
        && expected.last_write_time() == actual.last_write_time()
        && expected.file_size() == actual.file_size()
}

/// Open a source regular file for reading without following a symlink at
/// `path`. Fails closed when the entry is a link or changes identity between
/// classification and open.
#[cfg(unix)]
fn open_source(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = OpenOptions::new();
    opts.read(true);
    match O_NOFOLLOW_FLAG {
        Some(flag) => {
            opts.custom_flags(flag);
            opts.open(path)
        }
        None => {
            let before = fs::symlink_metadata(path)?;
            if before.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "source entry is a symlink",
                ));
            }
            let file = opts.open(path)?;
            if file_id(&file.metadata()?) != file_id(&before) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "source entry changed identity between scan and open",
                ));
            }
            Ok(file)
        }
    }
}

#[cfg(windows)]
fn open_source(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if file.metadata()?.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source entry is a symlink",
        ));
    }
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
compile_error!(
    "dir-tree-ops needs a no-follow source open, which is defined for Unix and Windows only"
);

/// Confirm that `sp` still names the file `checked` was opened on before
/// `std::fs::copy` ran. The copy reads the source through its own
/// path-following open, so a path swapped to a symlink after the pre-copy
/// check would feed it foreign bytes. Re-open no-follow and compare
/// identity against the held handle. A reopen failure or a mismatch is a
/// swap and fails closed as `InvalidInput`. The caller removes the
/// destination file on that error.
fn confirm_copied_source(sp: &Path, checked: &File) -> io::Result<()> {
    let swapped = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "source entry changed identity during copy",
        )
    };
    let reopened = open_source(sp).map_err(|_| swapped())?;
    let before = checked.metadata()?;
    let after = reopened.metadata().map_err(|_| swapped())?;
    if !same_file_identity(&before, &after) {
        return Err(swapped());
    }
    Ok(())
}

#[cfg(unix)]
fn fsync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn fsync_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn make_symlink(target: &Path, link: &Path) -> io::Result<()> {
    use std::os::windows::fs as wfs;
    let resolved = match link.parent() {
        Some(parent) => parent.join(target),
        None => target.to_path_buf(),
    };
    match fs::metadata(&resolved) {
        Ok(meta) if meta.is_dir() => wfs::symlink_dir(target, link),
        _ => wfs::symlink_file(target, link),
    }
}

fn nonce() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()) ^ d.as_secs())
        .unwrap_or(0);
    t ^ COUNTER
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

fn call_tag() -> String {
    format!("{}.{:x}", std::process::id(), nonce())
}

/// A short hidden sibling name that stays valid when `dst` is at `NAME_MAX`.
fn hidden_sibling_name(tag: &str, ext: &str) -> OsString {
    let mut s = OsString::from(".");
    s.push(tag);
    s.push(".");
    s.push(ext);
    s
}

/// Canonicalize the longest existing prefix of `p` and append the rest.
fn resolve_existing(p: &Path) -> PathBuf {
    let mut cur = p.to_path_buf();
    let mut tail: Vec<OsString> = Vec::new();
    loop {
        match cur.canonicalize() {
            Ok(mut resolved) => {
                for part in tail.iter().rev() {
                    resolved.push(part);
                }
                return resolved;
            }
            Err(_) => {
                let name = match cur.file_name() {
                    Some(n) => n.to_os_string(),
                    None => return p.to_path_buf(),
                };
                let parent = match cur.parent() {
                    Some(par) if !par.as_os_str().is_empty() => par.to_path_buf(),
                    Some(_) => PathBuf::from("."),
                    None => return p.to_path_buf(),
                };
                tail.push(name);
                cur = parent;
            }
        }
    }
}

/// True when some ancestor-or-self of `path` has the identity `id`.
#[cfg(unix)]
fn path_has_ancestor_id(path: &Path, id: (u64, u64)) -> bool {
    path.ancestors()
        .any(|a| fs::metadata(a).map(|m| file_id(&m) == id).unwrap_or(false))
}

#[cfg(unix)]
fn aliased_by_identity(
    src_meta: &Metadata,
    src_resolved: &Path,
    dst: &Path,
    dst_resolved: &Path,
) -> bool {
    if path_has_ancestor_id(dst_resolved, file_id(src_meta)) {
        return true;
    }
    match fs::metadata(dst) {
        Ok(dst_meta) => path_has_ancestor_id(src_resolved, file_id(&dst_meta)),
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn aliased_by_identity(_: &Metadata, _: &Path, _: &Path, _: &Path) -> bool {
    false
}

/// Reject every aliasing relationship between the two roots before any
/// mutation: equal, either inside the other, by path after resolving the
/// existing portion and (Unix) by `(dev, ino)` along each path.
fn overlap_guard(src: &Path, src_meta: &Metadata, dst: &Path) -> io::Result<Option<Metadata>> {
    if src != dst && src.parent() == dst.parent() {
        match fs::symlink_metadata(dst) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Ok(meta) if meta.is_dir() && portable_file_id(&meta) != portable_file_id(src_meta) => {
                return Ok(Some(meta));
            }
            _ => {}
        }
    }
    let s = resolve_existing(src);
    let d = resolve_existing(dst);
    let aliased =
        d.starts_with(&s) || s.starts_with(&d) || aliased_by_identity(src_meta, &s, dst, &d);
    if aliased {
        return Err(contract(
            Op::Scan,
            dst,
            Some(src.to_path_buf()),
            io::ErrorKind::InvalidInput,
            "source and destination overlap: equal, nested, or aliased",
        ));
    }
    Ok(None)
}

/// Identities of every source entry. Existing destination entries are checked
/// against this set before mutation and again before each replacement.
struct SourceGuard {
    src: PathBuf,
    root_id: Option<(u64, u64)>,
    ids: Option<HashSet<(u64, u64)>>,
}

impl SourceGuard {
    fn new(src: &Path, src_meta: &Metadata) -> Self {
        SourceGuard {
            src: src.to_path_buf(),
            root_id: portable_file_id(src_meta),
            ids: None,
        }
    }

    fn collect_source_identities(&mut self) -> io::Result<()> {
        let mut ids = HashSet::new();
        if let Some(root_id) = self.root_id {
            ids.insert(root_id);
        }
        self.ids = Some(ids);
        let src = self.src.clone();
        let mut stack = vec![src];
        while let Some(dir) = stack.pop() {
            let entries = read_dir(&dir)?;
            for entry in entries {
                let entry = entry.map_err(|e| wrap(Op::Scan, &dir, None, e))?;
                let path = entry.path();
                let meta =
                    fs::symlink_metadata(&path).map_err(|e| wrap(Op::Scan, &path, None, e))?;
                self.add_identity(&meta);
                if meta.is_dir() {
                    stack.push(path);
                }
            }
        }
        Ok(())
    }

    fn add_identity(&mut self, meta: &Metadata) {
        if let (Some(ids), Some(id)) = (&mut self.ids, portable_file_id(meta)) {
            ids.insert(id);
        }
    }

    /// Reject any existing destination entry that aliases a source entry.
    /// This pass completes before the first destination mutation.
    fn preflight_destination(&mut self, dst: &Path) -> io::Result<()> {
        let root_meta = match fs::symlink_metadata(dst) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(wrap(Op::Scan, dst, None, e)),
        };
        self.collect_source_identities()?;
        self.check(dst, &root_meta)?;
        if !root_meta.is_dir() {
            return Ok(());
        }
        let mut stack = vec![dst.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries = read_dir(&dir)?;
            for entry in entries {
                let entry = entry.map_err(|e| wrap(Op::Scan, &dir, None, e))?;
                let path = entry.path();
                let meta =
                    fs::symlink_metadata(&path).map_err(|e| wrap(Op::Scan, &path, None, e))?;
                self.check(&path, &meta)?;
                if meta.is_dir() {
                    stack.push(path);
                }
            }
        }
        Ok(())
    }

    fn check(&self, path: &Path, meta: &Metadata) -> io::Result<()> {
        if portable_file_id(meta)
            .map(|id| {
                self.root_id == Some(id)
                    || self
                        .ids
                        .as_ref()
                        .map(|ids| ids.contains(&id))
                        .unwrap_or(false)
            })
            .unwrap_or(false)
        {
            return Err(contract(
                Op::Scan,
                path,
                Some(self.src.clone()),
                io::ErrorKind::InvalidInput,
                "destination entry aliases the source tree",
            ));
        }
        Ok(())
    }

    /// Remove an existing destination entry with the primitive for its type.
    /// `path` is always a path the walker built under `dst`.
    fn remove_dest_entry(&self, path: &Path) -> io::Result<()> {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(wrap(Op::Scan, path, None, e)),
        };
        self.check(path, &meta)?;
        if meta.is_dir() {
            fs::remove_dir_all(path).map_err(|e| wrap(Op::RemoveEntry, path, None, e))
        } else {
            fs::remove_file(path).map_err(|e| wrap(Op::RemoveEntry, path, None, e))
        }
    }
}

fn read_dir(path: &Path) -> io::Result<fs::ReadDir> {
    fs::read_dir(path).map_err(|e| wrap(Op::Scan, path, None, e))
}

struct ProgressSink<'a> {
    cb: &'a mut dyn FnMut(&Progress<'_>),
    bytes_copied: u64,
    total_bytes: u64,
    entries_done: u64,
    entries_total: u64,
}

impl<'a> ProgressSink<'a> {
    fn new(cb: &'a mut dyn FnMut(&Progress<'_>), totals: &ScanTotals) -> ProgressSink<'a> {
        ProgressSink {
            cb,
            bytes_copied: 0,
            total_bytes: totals.bytes,
            entries_done: 0,
            entries_total: totals.entries,
        }
    }

    fn emit(&mut self, path: &Path) {
        let snapshot = Progress {
            bytes_copied: self.bytes_copied,
            total_bytes: self.total_bytes,
            entries_done: self.entries_done,
            entries_total: self.entries_total,
            current_path: path,
        };
        (self.cb)(&snapshot);
    }
}

struct ScanTotals {
    bytes: u64,
    files: u64,
    dirs: u64,
    symlinks: u64,
    entries: u64,
}

fn add_bytes(total: u64, amount: u64) -> u64 {
    total.saturating_add(amount)
}

impl ScanTotals {
    fn summary(&self) -> Summary {
        Summary {
            bytes_copied: self.bytes,
            files: self.files,
            dirs: self.dirs,
            symlinks: self.symlinks,
            skipped: 0,
        }
    }
}

/// Iterative pre-scan for progress totals. Counts the root directory
/// itself. Entry types the copy cannot reproduce are left out of the counts,
/// and the copy reports them when it reaches them.
fn scan_tree(root: &Path) -> io::Result<ScanTotals> {
    let mut totals = ScanTotals {
        bytes: 0,
        files: 0,
        dirs: 1,
        symlinks: 0,
        entries: 0,
    };
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = fs::read_dir(&dir).map_err(|e| wrap(Op::Scan, &dir, None, e))?;
        for entry in rd {
            let entry = entry.map_err(|e| wrap(Op::Scan, &dir, None, e))?;
            let meta = fs::symlink_metadata(entry.path())
                .map_err(|e| wrap(Op::Scan, &entry.path(), None, e))?;
            let ft = meta.file_type();
            if ft.is_symlink() {
                totals.symlinks += 1;
            } else if ft.is_dir() {
                totals.dirs += 1;
                stack.push(entry.path());
            } else if ft.is_file() {
                totals.files += 1;
                totals.bytes = add_bytes(totals.bytes, meta.len());
            }
        }
    }
    totals.entries = totals
        .files
        .saturating_add(totals.dirs)
        .saturating_add(totals.symlinks);
    Ok(totals)
}

/// Counts for a tree that already arrived by rename. An unreadable directory
/// counts as one directory.
fn summarize_moved(root: &Path) -> Summary {
    let mut summary = Summary {
        dirs: 1,
        ..Summary::default()
    };
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            let meta = match fs::symlink_metadata(entry.path()) {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            let ft = meta.file_type();
            if ft.is_symlink() {
                summary.symlinks += 1;
            } else if ft.is_dir() {
                summary.dirs += 1;
                stack.push(entry.path());
            } else if ft.is_file() {
                summary.files += 1;
                summary.bytes_copied = add_bytes(summary.bytes_copied, meta.len());
            }
        }
    }
    summary
}

type FileHook<'a> = &'a mut dyn FnMut(u64, &Path) -> io::Result<()>;
type PathHook<'a> = &'a mut dyn FnMut(&Path) -> io::Result<()>;
type VoidHook<'a> = &'a mut dyn FnMut();

struct Frame {
    src: PathBuf,
    dst: PathBuf,
    entries: fs::ReadDir,
    perms: fs::Permissions,
    created: bool,
}

struct CopyEngine<'a> {
    overwrite: Overwrite,
    fsync: bool,
    buf: Vec<u8>,
    summary: Summary,
    progress: Option<ProgressSink<'a>>,
    file_hook: Option<FileHook<'a>>,
    files_seen: u64,
    guard: SourceGuard,
}

impl CopyEngine<'_> {
    fn entry_done(&mut self, path: &Path) {
        if let Some(sink) = &mut self.progress {
            sink.entries_done += 1;
            sink.emit(path);
        }
    }

    /// Copy the children of `src_root` into `dst_root`, then apply
    /// `root_perms` if the caller created the root.
    fn run(
        &mut self,
        src_root: &Path,
        dst_root: &Path,
        root_created: bool,
        root_perms: fs::Permissions,
    ) -> io::Result<()> {
        let entries = read_dir(src_root)?;
        let mut frames = vec![Frame {
            src: src_root.to_path_buf(),
            dst: dst_root.to_path_buf(),
            entries,
            perms: root_perms,
            created: root_created,
        }];
        while let Some(top) = frames.last_mut() {
            let next = top.entries.next();
            match next {
                None => {
                    let frame = frames.pop().expect("frame stack non-empty");
                    if frame.created {
                        fs::set_permissions(&frame.dst, frame.perms)
                            .map_err(|e| wrap(Op::CreateDir, &frame.dst, None, e))?;
                    }
                    if self.fsync {
                        fsync_dir(&frame.dst)
                            .map_err(|e| wrap(Op::CreateDir, &frame.dst, None, e))?;
                    }
                    self.entry_done(&frame.dst);
                }
                Some(entry) => {
                    let entry = entry.map_err(|e| {
                        let top = frames.last().expect("frame stack non-empty");
                        wrap(Op::Scan, &top.src, None, e)
                    })?;
                    let name = entry.file_name();
                    let (sp, dp) = {
                        let top = frames.last().expect("frame stack non-empty");
                        (top.src.join(&name), top.dst.join(&name))
                    };
                    let meta =
                        fs::symlink_metadata(&sp).map_err(|e| wrap(Op::Scan, &sp, None, e))?;
                    self.guard.add_identity(&meta);
                    let ft = meta.file_type();
                    if ft.is_symlink() {
                        self.copy_symlink(&sp, &dp)?;
                    } else if ft.is_dir() {
                        if let Some(frame) = self.enter_dir(&sp, &dp, meta)? {
                            frames.push(frame);
                        }
                    } else if ft.is_file() {
                        self.copy_file(&sp, &dp, &meta)?;
                    } else {
                        return Err(contract(
                            Op::CopyFile,
                            &sp,
                            None,
                            io::ErrorKind::Unsupported,
                            "unsupported file type",
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn enter_dir(&mut self, sp: &Path, dp: &Path, meta: Metadata) -> io::Result<Option<Frame>> {
        let created = match fs::create_dir(dp) {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let meta = fs::symlink_metadata(dp).map_err(|e2| wrap(Op::Scan, dp, None, e2))?;
                if meta.is_dir() {
                    // The source guard cleared this existing directory for merge.
                    self.guard.check(dp, &meta)?;
                    false
                } else {
                    match self.overwrite {
                        Overwrite::Error => {
                            return Err(wrap(Op::CreateDir, dp, Some(sp.to_path_buf()), e))
                        }
                        Overwrite::Skip => {
                            self.summary.skipped += 1;
                            self.entry_done(dp);
                            return Ok(None);
                        }
                        Overwrite::Replace => {
                            self.guard.remove_dest_entry(dp)?;
                            fs::create_dir(dp).map_err(|e2| wrap(Op::CreateDir, dp, None, e2))?;
                            true
                        }
                    }
                }
            }
            Err(e) => return Err(wrap(Op::CreateDir, dp, None, e)),
        };
        if created {
            self.summary.dirs += 1;
        }
        let entries = read_dir(sp)?;
        Ok(Some(Frame {
            src: sp.to_path_buf(),
            dst: dp.to_path_buf(),
            entries,
            perms: meta.permissions(),
            created,
        }))
    }

    fn copy_symlink(&mut self, sp: &Path, dp: &Path) -> io::Result<()> {
        let target = fs::read_link(sp).map_err(|e| wrap(Op::Scan, sp, None, e))?;
        match make_symlink(&target, dp) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => match self.overwrite {
                Overwrite::Error => {
                    return Err(wrap(Op::Symlink, dp, Some(sp.to_path_buf()), e));
                }
                Overwrite::Skip => {
                    self.summary.skipped += 1;
                    self.entry_done(dp);
                    return Ok(());
                }
                Overwrite::Replace => {
                    self.guard.remove_dest_entry(dp)?;
                    make_symlink(&target, dp)
                        .map_err(|e2| wrap(Op::Symlink, dp, Some(sp.to_path_buf()), e2))?;
                }
            },
            Err(e) => return Err(wrap(Op::Symlink, dp, Some(sp.to_path_buf()), e)),
        }
        self.summary.symlinks += 1;
        self.entry_done(dp);
        Ok(())
    }

    /// Claim the destination slot for one file copy with `create_new`.
    /// `Ok(None)` means the entry was skipped under [`Overwrite::Skip`].
    /// Errors are already wrapped.
    fn open_dest(&mut self, sp: &Path, dp: &Path) -> io::Result<Option<File>> {
        let create = |dp: &Path| OpenOptions::new().write(true).create_new(true).open(dp);
        match create(dp) {
            Ok(file) => Ok(Some(file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => match self.overwrite {
                Overwrite::Error => Err(wrap(Op::CopyFile, sp, Some(dp.to_path_buf()), e)),
                Overwrite::Skip => Ok(None),
                Overwrite::Replace => {
                    self.guard.remove_dest_entry(dp)?;
                    match create(dp) {
                        Ok(file) => Ok(Some(file)),
                        Err(e2) => Err(wrap(Op::CopyFile, sp, Some(dp.to_path_buf()), e2)),
                    }
                }
            },
            Err(e) => Err(wrap(Op::CopyFile, sp, Some(dp.to_path_buf()), e)),
        }
    }

    fn prepare_fast_dest(&mut self, sp: &Path, dp: &Path) -> io::Result<bool> {
        match fs::symlink_metadata(dp) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(e) => Err(wrap(Op::Scan, dp, Some(sp.to_path_buf()), e)),
            Ok(_) => match self.overwrite {
                Overwrite::Error => Err(contract(
                    Op::CopyFile,
                    sp,
                    Some(dp.to_path_buf()),
                    io::ErrorKind::AlreadyExists,
                    "destination already exists",
                )),
                Overwrite::Skip => Ok(false),
                Overwrite::Replace => {
                    self.guard.remove_dest_entry(dp)?;
                    Ok(true)
                }
            },
        }
    }

    fn copy_file(&mut self, sp: &Path, dp: &Path, scanned_meta: &Metadata) -> io::Result<()> {
        let ctx = |e: io::Error| wrap(Op::CopyFile, sp, Some(dp.to_path_buf()), e);
        self.files_seen += 1;
        if let Some(hook) = &mut self.file_hook {
            hook(self.files_seen, sp).map_err(ctx)?;
        }
        let src = open_source(sp).map_err(ctx)?;
        let meta = src.metadata().map_err(ctx)?;
        if !meta.is_file() || !same_file_identity(scanned_meta, &meta) {
            return Err(contract(
                Op::CopyFile,
                sp,
                Some(dp.to_path_buf()),
                io::ErrorKind::InvalidInput,
                "source entry changed identity after directory scan",
            ));
        }
        let written = if self.progress.is_none() {
            if !self.prepare_fast_dest(sp, dp)? {
                self.summary.skipped += 1;
                self.entry_done(dp);
                return Ok(());
            }
            self.copy_fast(sp, dp, src).map_err(ctx)?
        } else {
            let dst = match self.open_dest(sp, dp)? {
                Some(file) => file,
                None => {
                    self.summary.skipped += 1;
                    self.entry_done(dp);
                    return Ok(());
                }
            };
            self.copy_buffered(src, dst, &meta, dp).map_err(ctx)?
        };
        self.summary.bytes_copied = add_bytes(self.summary.bytes_copied, written);
        self.summary.files += 1;
        self.entry_done(dp);
        Ok(())
    }

    /// Progress path: stream through the buffer from the no-follow handle
    /// into the claimed destination handle, one callback per chunk.
    fn copy_buffered(
        &mut self,
        mut src: File,
        mut dst: File,
        meta: &Metadata,
        dp: &Path,
    ) -> io::Result<u64> {
        let mut buf = std::mem::take(&mut self.buf);
        let mut written = 0u64;
        let result = loop {
            let n = match src.read(&mut buf) {
                Ok(n) => n,
                Err(e) => break Err(e),
            };
            if n == 0 {
                break Ok(());
            }
            if let Err(e) = dst.write_all(&buf[..n]) {
                break Err(e);
            }
            written = add_bytes(written, n as u64);
            if let Some(sink) = &mut self.progress {
                sink.bytes_copied = add_bytes(sink.bytes_copied, n as u64);
                sink.emit(dp);
            }
        };
        self.buf = buf;
        result?;
        dst.set_permissions(meta.permissions())?;
        if self.fsync {
            dst.sync_all()?;
        }
        Ok(written)
    }

    /// Fast path: `std::fs::copy` creates the final file, then the source
    /// path is re-opened no-follow and compared against the handle held
    /// since the pre-copy check. Any failure removes the new file.
    fn copy_fast(&mut self, sp: &Path, dp: &Path, checked_src: File) -> io::Result<u64> {
        let result = fs::copy(sp, dp).and_then(|n| {
            confirm_copied_source(sp, &checked_src)?;
            if self.fsync {
                open_source(dp)?.sync_all()?;
            }
            Ok(n)
        });
        drop(checked_src);
        match result {
            Ok(written) => Ok(written),
            Err(copy_error) => match fs::remove_file(dp) {
                Ok(()) => Err(copy_error),
                Err(cleanup) if cleanup.kind() == io::ErrorKind::NotFound => Err(copy_error),
                Err(cleanup) => Err(wrap(Op::RemoveEntry, dp, None, cleanup)),
            },
        }
    }
}

enum RootPrep {
    Proceed { created: bool },
    Noop,
}

fn prepare_root(dst: &Path, overwrite: Overwrite, guard: &SourceGuard) -> io::Result<RootPrep> {
    match fs::create_dir(dst) {
        Ok(()) => Ok(RootPrep::Proceed { created: true }),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(dst).map_err(|e2| wrap(Op::Scan, dst, None, e2))?;
            if meta.is_dir() {
                guard.check(dst, &meta)?;
                Ok(RootPrep::Proceed { created: false })
            } else {
                match overwrite {
                    Overwrite::Error => Err(wrap(Op::CreateDir, dst, None, e)),
                    Overwrite::Skip => Ok(RootPrep::Noop),
                    Overwrite::Replace => {
                        guard.remove_dest_entry(dst)?;
                        fs::create_dir(dst).map_err(|e2| wrap(Op::CreateDir, dst, None, e2))?;
                        Ok(RootPrep::Proceed { created: true })
                    }
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let parent = dst.parent().unwrap_or(dst);
            Err(wrap(Op::CreateDir, parent, None, e))
        }
        Err(e) => Err(wrap(Op::CreateDir, dst, None, e)),
    }
}

fn source_dir_meta(src: &Path) -> io::Result<Metadata> {
    let meta = fs::symlink_metadata(src).map_err(|e| wrap(Op::Scan, src, None, e))?;
    if !meta.is_dir() {
        return Err(contract(
            Op::Scan,
            src,
            None,
            io::ErrorKind::InvalidInput,
            "source is not a directory; tree operations take directories",
        ));
    }
    Ok(meta)
}

/// Optimize the required single-file case without weakening merge behavior.
/// The destination root is claimed before the file copy, so its child slot is
/// private to this operation under the library's non-hostile caller model.
fn try_single_file_copy(
    src: &Path,
    dst: &Path,
    src_meta: &Metadata,
    known_dst: Option<&Metadata>,
) -> io::Result<Option<Summary>> {
    let mut entries = read_dir(src)?;
    let entry = match entries.next() {
        Some(entry) => entry.map_err(|e| wrap(Op::Scan, src, None, e))?,
        None => return Ok(None),
    };
    if entries.next().is_some() {
        return Ok(None);
    }
    let source_path = entry.path();
    let scanned = entry
        .metadata()
        .map_err(|e| wrap(Op::Scan, &source_path, None, e))?;
    if !scanned.is_file() {
        return Ok(None);
    }
    let checked = open_source(&source_path)
        .map_err(|e| wrap(Op::CopyFile, &source_path, Some(dst.to_path_buf()), e))?;
    let opened = checked
        .metadata()
        .map_err(|e| wrap(Op::CopyFile, &source_path, Some(dst.to_path_buf()), e))?;
    if !opened.is_file() || !same_file_identity(&scanned, &opened) {
        return Err(contract(
            Op::CopyFile,
            &source_path,
            Some(dst.to_path_buf()),
            io::ErrorKind::InvalidInput,
            "source entry changed identity after directory scan",
        ));
    }
    let created = match known_dst
        .cloned()
        .map(Ok)
        .unwrap_or_else(|| fs::symlink_metadata(dst))
    {
        Ok(meta) => {
            if !meta.is_dir() {
                return Ok(None);
            }
            let mut destination_entries = read_dir(dst)?;
            if destination_entries.next().is_some() {
                return Ok(None);
            }
            false
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(dst).map_err(|e| wrap(Op::CreateDir, dst, None, e))?;
            true
        }
        Err(e) => return Err(wrap(Op::Scan, dst, None, e)),
    };
    let destination_path = dst.join(entry.file_name());
    let copied = fs::copy(&source_path, &destination_path).map_err(|e| {
        wrap(
            Op::CopyFile,
            &source_path,
            Some(destination_path.clone()),
            e,
        )
    })?;
    if let Err(e) = confirm_copied_source(&source_path, &checked) {
        return Err(match fs::remove_file(&destination_path) {
            Ok(()) => wrap(
                Op::CopyFile,
                &source_path,
                Some(destination_path.clone()),
                e,
            ),
            Err(cleanup) if cleanup.kind() == io::ErrorKind::NotFound => wrap(
                Op::CopyFile,
                &source_path,
                Some(destination_path.clone()),
                e,
            ),
            Err(cleanup) => wrap(Op::RemoveEntry, &destination_path, None, cleanup),
        });
    }
    drop(checked);
    if created {
        fs::set_permissions(dst, src_meta.permissions())
            .map_err(|e| wrap(Op::CreateDir, dst, None, e))?;
    }
    Ok(Some(Summary {
        bytes_copied: copied,
        files: 1,
        dirs: u64::from(created),
        symlinks: 0,
        skipped: 0,
    }))
}

fn copy_tree_impl<'a>(
    src: &Path,
    dst: &Path,
    opts: &Options,
    mut progress_cb: Option<&'a mut dyn FnMut(&Progress<'_>)>,
    file_hook: Option<FileHook<'a>>,
) -> io::Result<Summary> {
    let src_meta = source_dir_meta(src)?;
    let known_dst = overlap_guard(src, &src_meta, dst)?;
    if progress_cb.is_none() && file_hook.is_none() {
        if let Some(summary) = try_single_file_copy(src, dst, &src_meta, known_dst.as_ref())? {
            return Ok(summary);
        }
    }
    let sink = match progress_cb.take() {
        Some(cb) => {
            let totals = scan_tree(src)?;
            let mut sink = ProgressSink::new(cb, &totals);
            sink.emit(dst);
            Some(sink)
        }
        None => None,
    };
    let mut guard = SourceGuard::new(src, &src_meta);
    guard.preflight_destination(dst)?;
    let root_created = match prepare_root(dst, opts.overwrite, &guard)? {
        RootPrep::Noop => return Ok(Summary::default()),
        RootPrep::Proceed { created } => created,
    };
    let buf = if sink.is_some() {
        vec![0; opts.buffer_size.max(1)]
    } else {
        Vec::new()
    };
    let mut engine = CopyEngine {
        overwrite: opts.overwrite,
        fsync: false,
        buf,
        summary: Summary::default(),
        progress: sink,
        file_hook,
        files_seen: 0,
        guard,
    };
    if root_created {
        engine.summary.dirs += 1;
    }
    engine.run(src, dst, root_created, src_meta.permissions())?;
    Ok(engine.summary)
}

/// Test hooks for the staged move path.
pub(crate) struct MoveCtl<'a> {
    /// Skip the rename attempt so the staged path runs on one filesystem.
    pub(crate) force_staged: bool,
    /// Called before each regular file copy with the 1-based file ordinal.
    pub(crate) file_hook: Option<FileHook<'a>>,
    /// Called after finalize, before source removal.
    pub(crate) post_finalize: Option<VoidHook<'a>>,
    /// Called before the aside removal of a replaced destination. An error
    /// stands in for a failed removal.
    pub(crate) aside_removal: Option<PathHook<'a>>,
    /// Replaces destination-parent sync in tests. An error must roll final
    /// placement back before the function returns.
    pub(crate) parent_sync: Option<PathHook<'a>>,
}

impl MoveCtl<'_> {
    fn none() -> Self {
        MoveCtl {
            force_staged: false,
            file_hook: None,
            post_finalize: None,
            aside_removal: None,
            parent_sync: None,
        }
    }
}

/// What sits at `dst` before a move, from `symlink_metadata`.
#[derive(Clone, Copy)]
enum DstKind {
    Dir,
    Other,
}

fn move_tree_impl<'a>(
    src: &Path,
    dst: &Path,
    opts: &Options,
    mut progress_cb: Option<&'a mut dyn FnMut(&Progress<'_>)>,
    ctl: MoveCtl<'a>,
) -> io::Result<Summary> {
    let src_meta = source_dir_meta(src)?;
    overlap_guard(src, &src_meta, dst)?;
    let existing = match fs::symlink_metadata(dst) {
        Ok(meta) if meta.is_dir() => Some(DstKind::Dir),
        Ok(_) => Some(DstKind::Other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(wrap(Op::Scan, dst, None, e)),
    };
    // The pre-scan runs only when the caller asked for totals. The
    // non-progress path issues the rename before reading the source.
    let (mut sink, totals) = match progress_cb.take() {
        Some(cb) => {
            let totals = scan_tree(src)?;
            let mut sink = ProgressSink::new(cb, &totals);
            sink.emit(dst);
            (Some(sink), Some(totals))
        }
        None => (None, None),
    };
    let replace = match (existing, opts.overwrite) {
        (None, _) => None,
        (Some(_), Overwrite::Error) => {
            return Err(contract(
                Op::Finalize,
                dst,
                Some(src.to_path_buf()),
                io::ErrorKind::AlreadyExists,
                "destination already exists",
            ));
        }
        (Some(_), Overwrite::Skip) => return Ok(Summary::default()),
        (Some(kind), Overwrite::Replace) => Some(kind),
    };
    if replace.is_none() && !ctl.force_staged {
        match fs::rename(src, dst) {
            Ok(()) => {
                let summary = match &totals {
                    Some(totals) => totals.summary(),
                    None => summarize_moved(dst),
                };
                if let Some(sink) = &mut sink {
                    sink.bytes_copied = summary.bytes_copied;
                    sink.entries_done = sink.entries_total;
                    sink.emit(dst);
                }
                return Ok(summary);
            }
            Err(e) if e.raw_os_error() == Some(CROSS_DEVICE_RAW) => {
                // Cross-device: fall through to the staged path.
            }
            Err(e) => return Err(wrap(Op::Rename, src, Some(dst.to_path_buf()), e)),
        }
    }
    staged_move(src, dst, opts, &src_meta, sink, replace, ctl)
}

#[cfg(unix)]
fn make_created_tree_removable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let meta = match fs::symlink_metadata(&dir) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if !meta.is_dir() {
            continue;
        }
        let mode = meta.permissions().mode();
        fs::set_permissions(&dir, fs::Permissions::from_mode(mode | 0o700))?;
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if fs::symlink_metadata(entry.path())?.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn make_created_tree_removable(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn cleanup_created_dir(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => {
            make_created_tree_removable(path).map_err(|e| wrap(Op::RemoveEntry, path, None, e))?;
            fs::remove_dir_all(path).map_err(|e| wrap(Op::RemoveEntry, path, None, e))
        }
    }
}

fn restore_parent(parent: &Path, permissions: &fs::Permissions) -> io::Result<()> {
    fs::set_permissions(parent, permissions.clone())
        .map_err(|e| wrap(Op::Finalize, parent, None, e))
}

fn sync_move_parent(ctl: &mut MoveCtl<'_>, parent: &Path) -> io::Result<()> {
    match ctl.parent_sync.as_mut() {
        Some(hook) => hook(parent),
        None => fsync_dir(parent),
    }
}

fn rollback_replacement(
    parent: &Path,
    parent_perms: &fs::Permissions,
    dst: &Path,
    aside: &Path,
    staging: &Path,
) -> io::Result<()> {
    restore_parent(parent, parent_perms)?;
    fs::rename(dst, staging)
        .map_err(|e| wrap(Op::Finalize, dst, Some(staging.to_path_buf()), e))?;
    fs::rename(aside, dst).map_err(|e| wrap(Op::Finalize, aside, Some(dst.to_path_buf()), e))?;
    cleanup_created_dir(staging)?;
    fsync_dir(parent).map_err(|e| wrap(Op::Finalize, parent, None, e))
}

fn rollback_new_destination(
    parent: &Path,
    parent_perms: &fs::Permissions,
    dst: &Path,
    staging: &Path,
) -> io::Result<()> {
    restore_parent(parent, parent_perms)?;
    fs::rename(dst, staging)
        .map_err(|e| wrap(Op::Finalize, dst, Some(staging.to_path_buf()), e))?;
    cleanup_created_dir(staging)?;
    fsync_dir(parent).map_err(|e| wrap(Op::Finalize, parent, None, e))
}

fn staged_move<'a>(
    src: &Path,
    dst: &Path,
    opts: &Options,
    src_meta: &Metadata,
    progress: Option<ProgressSink<'a>>,
    replace: Option<DstKind>,
    mut ctl: MoveCtl<'a>,
) -> io::Result<Summary> {
    let parent = match dst.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        Some(_) => PathBuf::from("."),
        None => {
            return Err(contract(
                Op::Finalize,
                dst,
                None,
                io::ErrorKind::InvalidInput,
                "destination has no parent directory",
            ));
        }
    };
    dst.file_name().ok_or_else(|| {
        contract(
            Op::Finalize,
            dst,
            None,
            io::ErrorKind::InvalidInput,
            "destination has no file name",
        )
    })?;
    let parent_perms = fs::symlink_metadata(&parent)
        .map_err(|e| wrap(Op::Scan, &parent, None, e))?
        .permissions();
    let mut guard = SourceGuard::new(src, src_meta);
    guard.preflight_destination(dst)?;
    let tag = call_tag();
    let staging = parent.join(hidden_sibling_name(&tag, "part"));
    fs::create_dir(&staging).map_err(|e| wrap(Op::CreateDir, &staging, None, e))?;
    let buf = if progress.is_some() {
        vec![0; opts.buffer_size.max(1)]
    } else {
        Vec::new()
    };
    let mut engine = CopyEngine {
        overwrite: Overwrite::Error,
        fsync: true,
        buf,
        summary: Summary {
            dirs: 1,
            ..Summary::default()
        },
        progress,
        file_hook: ctl.file_hook.take(),
        files_seen: 0,
        guard,
    };
    if let Err(e) = engine.run(src, &staging, true, src_meta.permissions()) {
        drop(engine);
        cleanup_created_dir(&staging)?;
        return Err(e);
    }
    let summary = engine.summary.clone();
    drop(engine);
    match replace {
        Some(kind) => {
            let aside = parent.join(hidden_sibling_name(&tag, "old"));
            if let Err(e) = fs::rename(dst, &aside) {
                cleanup_created_dir(&staging)?;
                return Err(wrap(Op::Finalize, dst, Some(aside), e));
            }
            if let Err(e) = fs::rename(&staging, dst) {
                restore_parent(&parent, &parent_perms)?;
                fs::rename(&aside, dst).map_err(|restore| {
                    wrap(Op::Finalize, &aside, Some(dst.to_path_buf()), restore)
                })?;
                cleanup_created_dir(&staging)?;
                fsync_dir(&parent).map_err(|sync| wrap(Op::Finalize, &parent, None, sync))?;
                return Err(wrap(Op::Finalize, &staging, Some(dst.to_path_buf()), e));
            }
            if let Err(e) = sync_move_parent(&mut ctl, &parent) {
                rollback_replacement(&parent, &parent_perms, dst, &aside, &staging)?;
                return Err(wrap(Op::Finalize, &parent, Some(dst.to_path_buf()), e));
            }
            let removed = match ctl.aside_removal.as_mut() {
                Some(hook) => hook(&aside),
                None => Ok(()),
            }
            .and_then(|()| match kind {
                DstKind::Dir => fs::remove_dir_all(&aside),
                DstKind::Other => fs::remove_file(&aside),
            });
            if let Err(e) = removed {
                rollback_replacement(&parent, &parent_perms, dst, &aside, &staging)?;
                return Err(wrap(Op::RemoveEntry, &aside, Some(dst.to_path_buf()), e));
            }
        }
        None => {
            if let Err(e) = fs::rename(&staging, dst) {
                cleanup_created_dir(&staging)?;
                return Err(wrap(Op::Finalize, &staging, Some(dst.to_path_buf()), e));
            }
            if let Err(e) = sync_move_parent(&mut ctl, &parent) {
                rollback_new_destination(&parent, &parent_perms, dst, &staging)?;
                return Err(wrap(Op::Finalize, &parent, Some(dst.to_path_buf()), e));
            }
        }
    }
    if let Some(hook) = ctl.post_finalize.as_mut() {
        hook();
    }
    remove_tree(src).map_err(|e| wrap(Op::RemoveSource, src, None, e))?;
    Ok(summary)
}

/// Recursively copy the directory tree at `src` to `dst`.
///
/// `dst` names the resulting root (`src/<rel>` maps to `dst/<rel>`). Regular
/// files are copied byte-for-byte through `std::fs::copy`, empty directories
/// are created, symlinks are reproduced verbatim and treated as leaves, and
/// Unix permission bits are preserved on files and directories. Overlapping
/// roots are rejected with `InvalidInput` before anything is touched. On
/// error, `src` stays unchanged, and partial destination output may
/// remain.
pub fn copy_tree(
    src: impl AsRef<Path>,
    dst: impl AsRef<Path>,
    opts: &Options,
) -> io::Result<Summary> {
    copy_tree_impl(src.as_ref(), dst.as_ref(), opts, None, None)
}

/// [`copy_tree`] with a progress callback.
///
/// Files stream through a buffer of [`Options::buffer_size`] bytes. The
/// callback runs once after the pre-scan, at most once per copied chunk,
/// and at least once at each entry's completion, zero-byte files included.
/// `bytes_copied` is non-decreasing, and totals are fixed by one pre-scan.
/// Callback panics propagate to the caller.
pub fn copy_tree_with_progress(
    src: impl AsRef<Path>,
    dst: impl AsRef<Path>,
    opts: &Options,
    mut on_progress: impl FnMut(&Progress<'_>),
) -> io::Result<Summary> {
    copy_tree_impl(
        src.as_ref(),
        dst.as_ref(),
        opts,
        Some(&mut on_progress),
        None,
    )
}

/// Move the directory tree at `src` to `dst` without ever endangering `src`.
///
/// Phases: a rename attempt when `dst` is absent, issued before any read of
/// `src`, so every entry type the filesystem holds moves with it. When the
/// rename fails across devices, or when an existing `dst` is being
/// replaced, a staged copy runs into a hidden sibling of `dst` (files
/// fsynced, directories fsynced on Unix), then a finalize rename, and only
/// then removal of `src`. A failure before source removal returns the error
/// with `src` unchanged, `dst` as it was, and no hidden sibling left under
/// the parent of `dst`. A failure during source removal reports
/// [`Op::RemoveSource`], and `dst` is already complete, so the call can be
/// retried.
///
/// An existing `dst` is handled as a whole: [`Overwrite::Error`] returns
/// `AlreadyExists`, [`Overwrite::Skip`] returns a zero summary with `src`
/// left in place, and [`Overwrite::Replace`] swaps the finished tree in and
/// then removes the old entry with the primitive for its type (a file or
/// link with `remove_file`, a directory with `remove_dir_all`), so the final
/// path always has either the old entry or a complete replacement available.
///
/// After a successful rename the summary is taken from a scan of `dst`. A
/// directory that scan cannot read counts as one directory and its contents
/// are left out.
pub fn move_tree(
    src: impl AsRef<Path>,
    dst: impl AsRef<Path>,
    opts: &Options,
) -> io::Result<Summary> {
    move_tree_impl(src.as_ref(), dst.as_ref(), opts, None, MoveCtl::none())
}

/// [`move_tree`] with a progress callback.
///
/// See [`copy_tree_with_progress`] for the callback contract. Totals need a
/// pre-scan of `src`, so this variant reads the source before the rename
/// attempt. When the rename fast path applies, the callback runs once after
/// the rename with `bytes_copied` equal to the pre-scanned byte size.
pub fn move_tree_with_progress(
    src: impl AsRef<Path>,
    dst: impl AsRef<Path>,
    opts: &Options,
    mut on_progress: impl FnMut(&Progress<'_>),
) -> io::Result<Summary> {
    move_tree_impl(
        src.as_ref(),
        dst.as_ref(),
        opts,
        Some(&mut on_progress),
        MoveCtl::none(),
    )
}

/// Recursively remove the tree at `path`.
///
/// Decisions come from `symlink_metadata` alone: if `path` is a symlink,
/// only the link is removed, and symlinks inside the tree are removed as
/// links, so a linked-in tree survives. A missing `path` is `NotFound`,
/// which includes reporting a broken symlink's removal honestly instead of
/// silently skipping it. Removal is not transactional: entries already
/// removed stay removed when a later entry fails.
pub fn remove_tree(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    let meta = fs::symlink_metadata(path).map_err(|e| wrap(Op::Scan, path, None, e))?;
    if !meta.is_dir() {
        return fs::remove_file(path).map_err(|e| wrap(Op::RemoveEntry, path, None, e));
    }
    let mut frames: Vec<(PathBuf, fs::ReadDir)> = vec![(path.to_path_buf(), read_dir(path)?)];
    while let Some(top) = frames.last_mut() {
        let next = top.1.next();
        match next {
            Some(entry) => {
                let dir = frames.last().expect("frame stack non-empty").0.clone();
                let entry = entry.map_err(|e| wrap(Op::Scan, &dir, None, e))?;
                let p = entry.path();
                let meta = fs::symlink_metadata(&p).map_err(|e| wrap(Op::Scan, &p, None, e))?;
                if meta.is_dir() {
                    let entries = read_dir(&p)?;
                    frames.push((p, entries));
                } else {
                    fs::remove_file(&p).map_err(|e| wrap(Op::RemoveEntry, &p, None, e))?;
                }
            }
            None => {
                let (dir, _) = frames.pop().expect("frame stack non-empty");
                fs::remove_dir(&dir).map_err(|e| wrap(Op::RemoveEntry, &dir, None, e))?;
            }
        }
    }
    Ok(())
}

/// Total size in bytes of the tree at `path`.
///
/// Sums regular-file byte lengths plus symlink target lengths (from
/// `symlink_metadata`). Directories contribute zero and links are leaves, so
/// a link into a large tree costs its link length and link cycles cannot
/// recurse. Values are byte lengths, not disk blocks, so they
/// differ from `du`.
pub fn tree_size(path: impl AsRef<Path>) -> io::Result<u64> {
    let path = path.as_ref();
    let meta = fs::symlink_metadata(path).map_err(|e| wrap(Op::Scan, path, None, e))?;
    if !meta.is_dir() {
        return Ok(meta.len());
    }
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = fs::read_dir(&dir).map_err(|e| wrap(Op::Scan, &dir, None, e))?;
        for entry in rd {
            let entry = entry.map_err(|e| wrap(Op::Scan, &dir, None, e))?;
            let meta = fs::symlink_metadata(entry.path())
                .map_err(|e| wrap(Op::Scan, &entry.path(), None, e))?;
            if meta.file_type().is_dir() {
                stack.push(entry.path());
            } else {
                total = add_bytes(total, meta.len());
            }
        }
    }
    Ok(total)
}
