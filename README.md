# dir-tree-ops

Recursive directory-tree operations: copy, move, remove, and size. Zero
runtime dependencies, `#![forbid(unsafe_code)]`, MSRV 1.70.

```rust
use dir-tree-ops::{copy_tree, move_tree, Options, Overwrite};

let opts = Options::default(); // conflict policy: error on existing entries
let summary = copy_tree("photos", "backup/photos", &opts)?;
println!("{} files, {} bytes", summary.files, summary.bytes_copied);

let opts = Options { overwrite: Overwrite::Replace, ..Options::default() };
move_tree("staging/site", "live/site", &opts)?;
# Ok::<(), std::io::Error>(())
```

## API

- `copy_tree(src, dst, &Options) -> io::Result<Summary>`
- `move_tree(src, dst, &Options) -> io::Result<Summary>`
- `copy_tree_with_progress(src, dst, &Options, FnMut(&Progress))`
- `move_tree_with_progress(src, dst, &Options, FnMut(&Progress))`
- `remove_tree(path) -> io::Result<()>`
- `tree_size(path) -> io::Result<u64>`

`Options` holds an `Overwrite` policy (`Error`, `Skip`, `Replace`) and a
buffer size for the progress variants. Progress callbacks see monotonic
byte counts against totals fixed by one pre-scan.

## Semantics

**Destination is the resulting root.** `copy_tree("a/src", "b/dest", ..)`
produces `b/dest` as a replica of `a/src`. The mapping is `src/<rel>` to
`dst/<rel>`, always, matching `rsync -a src/ dst/`. No name-appending, so
first and second runs produce the same shape. Equal or nested roots in either
direction are rejected with `InvalidInput` before anything is touched.
Existing destination entries are also rejected when a hard link or filesystem
alias gives them the identity of any source entry.

**Moves protect the source.** `move_tree` renames when it can. When
it cannot (cross-device, or replacing an existing destination), it copies
into a hidden staging sibling with per-file fsync, renames staging into
place, and only then removes the source. A failure before that last step
leaves the source unchanged. A failure during source removal reports the
phase, and the destination is already complete. The test suite injects
mid-move failures and asserts every source file survives.

**Symlinks are copied as symlinks.** Classification uses
`symlink_metadata`, the target string is reproduced byte-for-byte, and the
walker treats every link as a leaf, so link cycles terminate and a dangling
link copies as the same dangling link. `remove_tree` deletes a link as a
link: the tree behind it survives.

**Errors are `std::io::Error`.** The `io::ErrorKind` of the underlying
filesystem failure is preserved, and its payload downcasts to `PathError`
for the operation step and the path(s) involved. Invalid options return
`InvalidInput` before any filesystem access. All copy and move functions
reject buffer sizes above `isize::MAX`.

**Traversal is iterative and streaming.** Each stack frame holds a live
directory reader. The walker does not recurse or collect a directory into a
width-sized list. Progress copies claim new files with `create_new`.
Non-progress copies open the source no-follow, check its identity against
the scan, run `std::fs::copy`, and re-check the source identity afterward.
A source swapped mid-copy fails the call with `InvalidInput` and the new
destination file is removed before the error returns. A concurrent writer
that replaces whole directories on the traversal path can still redirect
descent.

## Trust model

The caller is non-hostile and operates on real filesystem paths. The library
handles large trees, permission failures, interrupted staged moves, symlink
cycles, path and inode overlap, and source or destination symlink swaps. A
swap detected during a file copy fails closed: the transient destination
file is removed and the call errors. It does not inspect mount tables or
defend against a privileged process that constructs new path aliases during
an operation, and a swap reverted to the original file before the post-copy
re-check is not detected.

## What it does not do (v1)

mtime, ownership, and xattr preservation, following links, cancellation
from the progress callback, single-file helpers, and async. Copying a FIFO,
socket, or device node reports `io::ErrorKind::Unsupported`. A same-filesystem
move carries those entries through the rename path. Unix permission bits are
preserved on files and directories, and a read-only directory gets its mode
applied after its contents are copied.

## Testing

The suite covers buffer-size validation before filesystem access, copy shape
and content, dangling links and cycles, source preservation under injected move
failures, progress monotonicity, structured errors, non-UTF-8 names,
4,096-deep chains, 10,000-file mutation soaks, and a mid-copy source-swap
soak. Symlink and permission tests need a Unix runner. The rest are
portable.

## License

MIT
