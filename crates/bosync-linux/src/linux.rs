//! Linux FUSE projection of a bosync proxy. Only compiled on Linux (see `lib.rs`).
//!
//! [`BosyncFs`] projects a [`GitBackend`]'s `HEAD` tree as a read-only filesystem: directory
//! listings come from git trees and file reads stream git blobs (hydration on demand). This is
//! the Linux counterpart to the Windows Cloud Filter provider. Write-back is not wired through
//! FUSE yet; the [`LinuxCloudSync`] seam exists so the shared engine compiles and so the
//! commit-on-save path can be grown next.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
    Request,
};

use bosync_core::CloudSync;
use bosync_git::GitBackend;

/// How long the kernel may cache attributes/entries before re-asking us. Short, because a
/// refresh of the proxy can change the tree underneath.
const TTL: Duration = Duration::from_secs(1);
/// FUSE reserves inode 1 for the mount root.
const ROOT_INO: u64 = 1;

/// The Linux implementation of [`bosync_core::CloudSync`]. FUSE entries are virtual (their
/// bytes live in git, not on disk), so there is no "dehydrated placeholder" notion to report
/// and no on-disk overlay to mark — every hook is a no-op for now. The type exists so the
/// shared engine has a platform seam to drive once FUSE write-back lands.
#[derive(Debug, Default, Clone, Copy)]
pub struct LinuxCloudSync;

impl CloudSync for LinuxCloudSync {
    fn is_dehydrated(&self, _abs: &Path) -> bool {
        false
    }
}

/// A read-only FUSE filesystem backed by a git tree.
///
/// Inodes are assigned lazily and remembered in both directions so the kernel's `lookup` ->
/// `getattr`/`read` calls (which speak in inodes) map back to git paths.
pub struct BosyncFs {
    git: GitBackend,
    /// inode -> git path (the empty string is the root).
    inodes: HashMap<u64, String>,
    /// git path -> inode (reverse of `inodes`, to reuse inodes across lookups).
    by_path: HashMap<String, u64>,
    next_ino: u64,
}

impl BosyncFs {
    pub fn new(git: GitBackend) -> Self {
        let mut inodes = HashMap::new();
        let mut by_path = HashMap::new();
        inodes.insert(ROOT_INO, String::new());
        by_path.insert(String::new(), ROOT_INO);
        Self {
            git,
            inodes,
            by_path,
            next_ino: ROOT_INO + 1,
        }
    }

    /// Return the inode for `path`, allocating a new one the first time it is seen.
    fn intern(&mut self, path: String) -> u64 {
        if let Some(&ino) = self.by_path.get(&path) {
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(ino, path.clone());
        self.by_path.insert(path, ino);
        ino
    }

    fn attr(&self, ino: u64, is_dir: bool, size: u64) -> FileAttr {
        let now = SystemTime::now();
        // SAFETY: getuid/getgid are always-succeeding libc calls with no preconditions.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: if is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if is_dir { 0o755 } else { 0o644 },
            nlink: if is_dir { 2 } else { 1 },
            uid,
            gid,
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    /// Join a child name onto a parent git path (`""` parent -> bare name).
    fn child_path(parent: &str, name: &str) -> String {
        if parent.is_empty() {
            name.to_string()
        } else {
            format!("{parent}/{name}")
        }
    }
}

impl Filesystem for BosyncFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_path) = self.inodes.get(&parent).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };
        let child = Self::child_path(&parent_path, &name.to_string_lossy());
        match self.git.stat(&child) {
            Some((is_dir, size)) => {
                let ino = self.intern(child);
                reply.entry(&TTL, &self.attr(ino, is_dir, size), 0);
            }
            None => reply.error(libc::ENOENT),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let Some(path) = self.inodes.get(&ino).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };
        match self.git.stat(&path) {
            Some((is_dir, size)) => reply.attr(&TTL, &self.attr(ino, is_dir, size)),
            None => reply.error(libc::ENOENT),
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let Some(path) = self.inodes.get(&ino).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };
        match self.git.read_blob(&path) {
            Ok(data) => {
                let start = (offset.max(0) as usize).min(data.len());
                let end = start.saturating_add(size as usize).min(data.len());
                reply.data(&data[start..end]);
            }
            Err(e) => {
                tracing::warn!(%path, "read: blob unavailable: {e:#}");
                reply.error(libc::ENOENT);
            }
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let Some(dir_path) = self.inodes.get(&ino).cloned() else {
            reply.error(libc::ENOENT);
            return;
        };

        // `.` and `..` first, then the git-tree children. Collect children before interning so
        // we don't hold an immutable borrow of `self.inodes` across the mutable `intern`.
        let mut entries: Vec<(u64, FileType, String)> = vec![
            (ino, FileType::Directory, ".".to_string()),
            (ROOT_INO, FileType::Directory, "..".to_string()),
        ];
        for e in self.git.list_dir(&dir_path).unwrap_or_default() {
            let path = Self::child_path(&dir_path, &e.name);
            let child_ino = self.intern(path);
            let kind = if e.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            entries.push((child_ino, kind, e.name));
        }

        // `offset` is the index of the next entry to emit; reply.add returns true when full.
        for (i, (child_ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(child_ino, (i + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }
}

/// Mount a git-backed read-only drive at `mountpoint` and block until it is unmounted
/// (Ctrl-C, or `fusermount3 -u <mountpoint>`).
pub fn mount(git: GitBackend, mountpoint: &Path) -> Result<()> {
    // Keep to options that work for an unprivileged mount without `user_allow_other`:
    // `AutoUnmount`/`AllowOther` require that system config and would otherwise fail the mount.
    let options = vec![MountOption::FSName("bosync".to_string()), MountOption::RO];
    tracing::info!(?mountpoint, "mounting bosync FUSE drive (read-only)");
    fuser::mount2(BosyncFs::new(git), mountpoint, &options)
        .with_context(|| format!("mounting FUSE filesystem at {mountpoint:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_is_stable_and_root_is_one() {
        // A throwaway backend; intern() doesn't touch git so an unborn repo is fine.
        let dir = std::env::temp_dir().join(format!("bosync-fs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let git = GitBackend::init_sample(&dir).unwrap();
        let mut fs = BosyncFs::new(git);

        assert_eq!(fs.by_path.get(""), Some(&ROOT_INO));
        let a = fs.intern("docs".to_string());
        let b = fs.intern("docs".to_string());
        assert_eq!(a, b, "same path -> same inode");
        assert_ne!(a, fs.intern("hello.txt".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
