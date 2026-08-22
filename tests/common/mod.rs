#![allow(dead_code)]

use std::fs;
use std::io;
use std::path::Path;

pub fn write_file(p: &Path, bytes: &[u8]) {
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(p, bytes).unwrap();
}

pub fn read(p: &Path) -> Vec<u8> {
    fs::read(p).unwrap()
}

#[cfg(unix)]
pub fn chmod(p: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
pub fn mode_of(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
}

#[cfg(unix)]
pub fn symlink(target: impl AsRef<Path>, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

pub fn is_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

pub fn msg_contains(err: &io::Error, p: &Path) -> bool {
    err.to_string().contains(&p.display().to_string())
}

pub fn path_ctx(err: &io::Error) -> &dir_tree_ops::PathError {
    err.get_ref()
        .expect("dir-tree-ops errors carry a payload")
        .downcast_ref()
        .expect("dir-tree-ops error payload is PathError")
}

pub fn is_dangling_symlink(p: &Path) -> bool {
    let meta = fs::symlink_metadata(p);
    matches!(meta, Ok(m) if m.file_type().is_symlink()) && fs::metadata(p).is_err()
}

/// One entry of a tree snapshot: kind plus the bytes or link target that
/// identify it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Node {
    File(Vec<u8>),
    Dir,
    Link(std::path::PathBuf),
    Other,
}

/// Byte-level snapshot of the tree at `root`, sorted by relative path, with
/// the root itself at the empty relative path. `None` when `root` is absent.
/// Links are recorded as leaves. A directory that cannot be read is
/// recorded as a directory with no children.
pub fn snapshot(root: &Path) -> Option<Vec<(std::path::PathBuf, Node)>> {
    let meta = fs::symlink_metadata(root).ok()?;
    let mut out = Vec::new();
    let mut stack = vec![(std::path::PathBuf::new(), meta)];
    while let Some((rel, meta)) = stack.pop() {
        let abs = root.join(&rel);
        let ft = meta.file_type();
        let node = if ft.is_symlink() {
            Node::Link(fs::read_link(&abs).unwrap())
        } else if ft.is_dir() {
            if let Ok(rd) = fs::read_dir(&abs) {
                for entry in rd.flatten() {
                    if let Ok(m) = fs::symlink_metadata(entry.path()) {
                        stack.push((rel.join(entry.file_name()), m));
                    }
                }
            }
            Node::Dir
        } else if ft.is_file() {
            Node::File(fs::read(&abs).unwrap())
        } else {
            Node::Other
        };
        out.push((rel, node));
    }
    out.sort();
    Some(out)
}

/// True when `parent` holds no entry named `.<dst_name>.*`.
pub fn no_aside_residue(parent: &Path, dst_name: &str) -> bool {
    let prefix = format!(".{dst_name}.");
    fs::read_dir(parent).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(&prefix)
    })
}
