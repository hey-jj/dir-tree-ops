# Changelog

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
