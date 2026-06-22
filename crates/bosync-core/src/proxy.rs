//! The on-disk proxy: a shallow clone of the remote that the OS projects as a virtual drive.
//!
//! This is the cross-platform base every platform builds on. It owns the lifecycle described
//! in the plan:
//!
//! 1. shallow-clone the remote into a per-user proxy folder ([`Proxy::open_or_clone`]),
//! 2. (the platform crate mounts that folder and marks entries cloud-only),
//! 3. on listing / open, refresh the shallow clone so the projection is current
//!    ([`Proxy::refresh`]),
//! 4. on save, reconcile the local change into a commit and push it back
//!    ([`Proxy::on_save`]),
//! 5. while idle, refetch, prune history to keep the proxy small, and re-dehydrate stale
//!    files ([`Proxy::refresh`], [`Proxy::prune`], [`Proxy::dehydrate_stale`]).
//!
//! It is generic over [`CloudSync`], so it has no Windows/Linux/macOS specifics.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use bosync_git::GitBackend;

use crate::mount::CloudSync;
use crate::reconcile::reconcile_path;

/// Default shallow-clone depth. One commit is enough to project the current tree; history is
/// pruned anyway, so we keep the proxy as small as possible.
pub const DEFAULT_DEPTH: u32 = 1;

/// An on-disk proxy backed by a shallow clone of `remote`.
pub struct Proxy {
    git: GitBackend,
    /// The on-disk proxy folder (the shallow clone's working tree) — also the sync root the
    /// platform mounts.
    root: PathBuf,
    /// The remote URL (`ssh://`, `git@host:...`, or `http(s)://`).
    remote: Box<str>,
    depth: u32,
}

impl Proxy {
    /// The per-user proxy directory bosync uses for `remote`, under the OS temp dir.
    ///
    /// The leaf is a sanitized, hash-suffixed slug of the URL so two different remotes never
    /// collide and the same remote is always reused. The OS temp dir is already per-user on
    /// every target platform, which is what the plan calls for.
    pub fn proxy_dir(remote: &str) -> PathBuf {
        let mut hasher = DefaultHasher::new();
        remote.hash(&mut hasher);
        let digest = hasher.finish();

        let slug: String = remote
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let slug: String = slug.trim_matches('-').chars().take(48).collect();

        std::env::temp_dir()
            .join("bosync")
            .join(format!("{slug}-{digest:016x}"))
    }

    /// Open the proxy for `remote`, shallow-cloning it if the proxy folder doesn't exist yet.
    /// Uses [`Self::proxy_dir`] for the location and [`DEFAULT_DEPTH`] for the depth.
    pub fn open_or_clone(remote: &str) -> Result<Self> {
        Self::open_or_clone_in(remote, &Self::proxy_dir(remote), DEFAULT_DEPTH)
    }

    /// As [`Self::open_or_clone`] but with an explicit proxy folder and depth (used by tests
    /// and by callers that want to control placement).
    pub fn open_or_clone_in(remote: &str, root: &Path, depth: u32) -> Result<Self> {
        let git = if root.join(".git").exists() {
            tracing::info!(?root, "reusing existing proxy");
            GitBackend::open(root)?
        } else {
            if let Some(parent) = root.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating proxy parent {parent:?}"))?;
            }
            tracing::info!(%remote, ?root, depth, "shallow-cloning into proxy");
            GitBackend::clone_shallow(remote, root, depth)?
        };
        Ok(Self {
            git,
            root: root.to_path_buf(),
            remote: remote.into(),
            depth,
        })
    }

    /// The proxy folder (the sync root the platform mounts).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The backing git handle, shared with the platform provider for hydration/projection.
    pub fn git(&self) -> &GitBackend {
        &self.git
    }

    /// Refresh the shallow clone from the remote so the projection is current. Called on
    /// directory listing and on the idle refetch tick.
    ///
    /// Redoes the shallow copy (per the plan), replacing the backing git handle. Anything
    /// sharing the previous [`Self::git`] handle (the platform provider) must re-acquire it.
    pub fn refresh(&mut self) -> Result<()> {
        self.git = GitBackend::refresh_shallow(&self.remote, &self.root, self.depth)?;
        Ok(())
    }

    /// On save of `abs`: reconcile the local change into a commit, then push it to the remote.
    ///
    /// Push is best-effort and reported separately from the commit so that a network failure
    /// doesn't lose the local commit — the next [`Self::refresh`]/save can retry the push.
    pub fn on_save<C: CloudSync>(&self, cloud: &C, abs: &Path) -> Result<()> {
        reconcile_path(&self.git, cloud, &self.root, abs)?;
        if let Err(e) = self.git.push(&self.remote) {
            tracing::warn!(remote = %self.remote, "push failed (commit kept locally): {e:#}");
        }
        Ok(())
    }

    /// Prune history to keep the proxy's git size down (idle maintenance).
    pub fn prune(&self) -> Result<()> {
        self.git.prune()
    }

    /// Re-dehydrate files under the proxy that haven't been accessed within `idle_for`,
    /// freeing their local bytes while keeping the placeholders visible. Best-effort; the
    /// actual eviction is the platform's job via [`CloudSync::dehydrate`].
    pub fn dehydrate_stale<C: CloudSync>(&self, cloud: &C, idle_for: Duration) {
        let now = SystemTime::now();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_dir() {
                    // Skip the git store itself; it is not part of the projected tree.
                    if path.file_name().is_some_and(|n| n == ".git") {
                        continue;
                    }
                    stack.push(path);
                    continue;
                }
                let idle = meta
                    .accessed()
                    .ok()
                    .and_then(|a| now.duration_since(a).ok())
                    .unwrap_or_default();
                if idle >= idle_for && !cloud.is_dehydrated(&path) {
                    cloud.dehydrate(&path);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TmpDir;

    #[test]
    fn proxy_dir_is_stable_and_distinct_per_remote() {
        let a1 = Proxy::proxy_dir("https://github.com/owner/repo.git");
        let a2 = Proxy::proxy_dir("https://github.com/owner/repo.git");
        let b = Proxy::proxy_dir("git@github.com:owner/other.git");
        assert_eq!(a1, a2, "same remote -> same proxy dir");
        assert_ne!(a1, b, "different remotes -> different proxy dirs");
        assert!(a1.starts_with(std::env::temp_dir().join("bosync")));
    }

    #[test]
    fn open_or_clone_reuses_an_existing_proxy() {
        // Stand in a local repo for the "remote": open_or_clone should reuse it (the folder
        // already has a .git), exercising the non-clone path without touching the network.
        let dir = TmpDir::new();
        GitBackend::init_sample(dir.path()).unwrap();

        let proxy = Proxy::open_or_clone_in("file://unused", dir.path(), DEFAULT_DEPTH).unwrap();
        assert_eq!(proxy.root(), dir.path());
        assert!(proxy.git().read_blob("hello.txt").is_ok());
    }
}
