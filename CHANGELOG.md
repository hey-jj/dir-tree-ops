# Changelog

## 0.1.1 - 2026-10-03

- `copy_tree_with_progress` and `move_tree_with_progress` now return
  `Err(InvalidInput)` when `buffer_size` exceeds the platform allocation limit,
  instead of panicking after the destination has been created. `copy_tree` and
  `move_tree` apply the same check before any filesystem access.

## 0.1.0 - 2026-08-21

First release.

- `copy_tree`, `move_tree`, `remove_tree`, `tree_size`, plus progress
  variants for copy and move.
- `Overwrite` policy enum (`Error`, `Skip`, `Replace`) for per-entry
  destination conflicts.
- Symlinks preserved verbatim and treated as leaves, dangling links included.
- Staged move: rename fast path, copy-fsync-finalize fallback, source
  untouched until the destination is complete.
- Errors are `std::io::Error` with a downcastable `PathError` payload.
- Zero runtime dependencies, `forbid(unsafe_code)`, MSRV 1.70.
