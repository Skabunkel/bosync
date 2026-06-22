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
    /// Laid out as `bosync/<creator>/<repo>` so several repos (and several creators) live
    /// side by side. `<creator>`/`<repo>` are the owner and repository name parsed from the
    /// remote URL. The OS temp dir is already per-user on every target platform, which is
    /// what the plan calls for.
    pub fn proxy_dir(remote: &str) -> PathBuf {
        let (creator, repo) = owner_repo(remote);
        std::env::temp_dir().join("bosync").join(creator).join(repo)
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

/// Parse the `(creator, repo)` pair out of a git remote URL, for the `bosync/<creator>/<repo>`
/// proxy layout. Handles both URL forms (`scheme://host/owner/repo[.git]`) and scp-like forms
/// (`git@host:owner/repo[.git]`). Components are sanitized to safe path segments; missing
/// parts fall back to `unknown` / `repo` so a malformed URL still yields a usable directory.
fn owner_repo(remote: &str) -> (String, String) {
    let trimmed = remote.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);

    // Reduce to the path portion after the host.
    let path = if let Some(idx) = trimmed.find("://") {
        let after = &trimmed[idx + 3..];
        after.split_once('/').map(|(_, p)| p).unwrap_or("")
    } else if let Some((_, p)) = trimmed.split_once(':') {
        p // scp-like `host:owner/repo`
    } else {
        trimmed
    };

    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let repo = parts.last().copied().unwrap_or("repo");
    let owner = if parts.len() >= 2 {
        parts[parts.len() - 2]
    } else {
        "unknown"
    };
    (sanitize_segment(owner), sanitize_segment(repo))
}

/// Make `s` a safe single path segment: keep alphanumerics, `-`, `_`, `.`; replace the rest
/// with `-`. Never empty.
fn sanitize_segment(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-');
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TmpDir;

    #[test]
    fn owner_repo_parses_url_and_scp_forms() {
        assert_eq!(
            owner_repo("https://github.com/owner/repo.git"),
            ("owner".into(), "repo".into())
        );
        assert_eq!(
            owner_repo("git@github.com:creator/myrepo.git"),
            ("creator".into(), "myrepo".into())
        );
        assert_eq!(
            owner_repo("ssh://git@host:22/team/project"),
            ("team".into(), "project".into())
        );
        // Trailing slash and no .git suffix.
        assert_eq!(
            owner_repo("https://gitlab.com/grp/sub/"),
            ("grp".into(), "sub".into())
        );
    }

    #[test]
    fn proxy_dir_groups_by_creator_then_repo() {
        let base = std::env::temp_dir().join("bosync");
        assert_eq!(
            Proxy::proxy_dir("https://github.com/owner/repo.git"),
            base.join("owner").join("repo")
        );
        // Same creator, different repos sit side by side; the same remote is stable.
        let a1 = Proxy::proxy_dir("git@github.com:owner/repo.git");
        let a2 = Proxy::proxy_dir("git@github.com:owner/repo.git");
        let b = Proxy::proxy_dir("git@github.com:owner/other.git");
        assert_eq!(a1, a2, "same remote -> same proxy dir");
        assert_ne!(a1, b, "different repo -> different proxy dir");
        assert_eq!(a1.parent(), b.parent(), "same creator -> shared parent dir");
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
