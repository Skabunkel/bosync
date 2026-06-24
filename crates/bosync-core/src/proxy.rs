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

use crate::mount::{CloudSync, ProxyState};
use crate::reconcile::{reconcile_path, Reconciled};

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
        // Normalize local `file://C:\…` remotes to a plain path — gix mishandles those URLs on
        // Windows. Stored as `self.remote` so clone, fetch and push all use the clean form.
        let remote: &str = &bosync_git::normalize_remote(remote);
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

    /// Bring the proxy up to date with `branch` and report which entries changed — the plan's
    /// "convert back to a shallow copy" step, run *in place* so it is safe while the drive is
    /// mounted.
    ///
    /// `git fetch --depth=1 origin <branch>` (via [`GitBackend::fetch_shallow`]) pulls the
    /// latest objects, stays shallow, and fast-forwards `HEAD` to the fetched tip. Because the
    /// live projection reads `HEAD` afresh on each callback, the new tree shows up immediately
    /// without re-acquiring the git handle.
    ///
    /// Returns the `/`-separated repo-relative paths whose blob changed or appeared (an empty
    /// vec if the remote hadn't moved). The platform layer maps these onto the drive and marks
    /// them [`Remote`](crate::ProxyState::Remote) so they re-dehydrate to cloud-only
    /// placeholders, keeping the proxy small.
    pub fn refresh_branch(&mut self, branch: &str) -> Result<Vec<String>> {
        let before = self.git.walk("HEAD").unwrap_or_default();
        if !self.git.fetch_shallow(&self.remote, branch, self.depth)? {
            return Ok(Vec::new()); // remote hasn't moved; nothing to pull down
        }
        let after = self.git.walk("HEAD").unwrap_or_default();
        self.git.prune()?; // best-effort compaction (gix has no gc yet)

        // The proxy folder *is* the working copy the user browses, so write the new content of
        // each changed entry into it. (Content equals the freshly fetched `HEAD`, so the
        // write-back watcher sees no diff and produces no spurious commit.)
        let changed = changed_paths(&before, &after);
        for rel in &changed {
            if let Ok(content) = self.git.read_blob(rel) {
                let full = join_rel(&self.root, rel);
                if let Some(parent) = full.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&full, content);
            }
        }
        Ok(changed)
    }

    /// On save of `abs`: reconcile the local change into a commit, then push it to the remote,
    /// driving the [`ProxyState`] overlay as it goes.
    ///
    /// The state transitions follow the plan exactly: a committed file is first marked
    /// [`Local`](ProxyState::Local) (present, but not yet confirmed on the remote), and only
    /// promoted to [`Synced`](ProxyState::Synced) once the push succeeds. If the push fails
    /// (or isn't supported yet), the commit is kept locally and the file stays `Local`, so the
    /// next save/refresh can retry the push without losing work — and the overlay honestly
    /// reflects "committed but not pushed".
    pub fn on_save<C: CloudSync>(&self, cloud: &C, abs: &Path) -> Result<()> {
        match reconcile_path(&self.git, cloud, &self.root, abs)? {
            Reconciled::Committed => {
                // Committed, push pending → Local.
                cloud.mark_state(&self.root, abs, ProxyState::Local);
                match self.git.push(&self.remote) {
                    // Pushed → confirmed identical to the remote.
                    Ok(()) => cloud.mark_state(&self.root, abs, ProxyState::Synced),
                    Err(e) => tracing::warn!(
                        remote = %self.remote,
                        "push failed (commit kept locally, file stays Local): {e:#}"
                    ),
                }
            }
            Reconciled::Removed => {
                // The removal is committed; push it so the remote drops the file too. Nothing
                // remains on disk to carry an overlay.
                if let Err(e) = self.git.push(&self.remote) {
                    tracing::warn!(remote = %self.remote, "push of removal failed (kept locally): {e:#}");
                }
            }
            Reconciled::Unchanged => {}
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

/// The repository name parsed from a remote URL — the last path segment, sans `.git`. Used to
/// derive a default drive folder name when the user doesn't pass one.
pub fn repo_name(remote: &str) -> String {
    owner_repo(remote).1
}

/// Join a `/`-separated repo-relative path onto a base directory, component by component, so it
/// is correct regardless of the platform path separator.
fn join_rel(base: &Path, rel: &str) -> PathBuf {
    let mut full = base.to_path_buf();
    for part in rel.split('/') {
        full.push(part);
    }
    full
}

/// The relative paths that changed between two `walk()` snapshots (`(path, oid, size)`): a path
/// is "changed" if its blob oid differs, or it is present in `after` but not `before`. Removed
/// paths are not returned — there is no on-disk entry left to re-dehydrate.
fn changed_paths(
    before: &[(String, String, u64)],
    after: &[(String, String, u64)],
) -> Vec<String> {
    use std::collections::HashMap;
    let prior: HashMap<&str, &str> = before
        .iter()
        .map(|(p, oid, _)| (p.as_str(), oid.as_str()))
        .collect();
    after
        .iter()
        .filter(|(path, oid, _)| prior.get(path.as_str()) != Some(&oid.as_str()))
        .map(|(path, _, _)| path.clone())
        .collect()
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
    fn changed_paths_reports_modified_and_new_only() {
        let before = vec![
            ("a.txt".to_string(), "oid_a".to_string(), 1),
            ("b.txt".to_string(), "oid_b".to_string(), 1),
            ("gone.txt".to_string(), "oid_g".to_string(), 1),
        ];
        let after = vec![
            ("a.txt".to_string(), "oid_a".to_string(), 1), // unchanged
            ("b.txt".to_string(), "oid_b2".to_string(), 1), // modified
            ("new.txt".to_string(), "oid_n".to_string(), 1), // added
        ];
        let mut changed = changed_paths(&before, &after);
        changed.sort();
        assert_eq!(changed, vec!["b.txt".to_string(), "new.txt".to_string()]);
    }

    #[test]
    fn on_save_keeps_file_local_when_push_is_unavailable() {
        use crate::testutil::RecordingCloudSync;
        use crate::ProxyState;

        // Stand in a local repo for the remote so open_or_clone reuses it (no network).
        let dir = TmpDir::new();
        GitBackend::init_sample(dir.path()).unwrap();
        let proxy = Proxy::open_or_clone_in("file://unused", dir.path(), DEFAULT_DEPTH).unwrap();

        let cloud = RecordingCloudSync::default();
        let file = dir.path().join("hello.txt");
        std::fs::write(&file, b"edited locally\n").unwrap();

        proxy.on_save(&cloud, &file).unwrap();

        // Committed, but push isn't supported yet → the file is Local, never promoted to Synced.
        assert_eq!(cloud.state_of(&file), Some(ProxyState::Local));
        assert_eq!(proxy.git().read_blob("hello.txt").unwrap(), b"edited locally\n");
    }

    #[test]
    fn refresh_branch_detects_remote_changes_and_stays_shallow() {
        // A local sample repo stands in for the remote (forward slashes: gix's local transport
        // wants a URL-ish path, not a backslashed Windows path).
        let remote_dir = TmpDir::new();
        GitBackend::init_sample(remote_dir.path()).unwrap();
        let remote_url = remote_dir.path().to_string_lossy().replace('\\', "/");

        // The proxy: a shallow clone of that "remote".
        let proxy_parent = TmpDir::new();
        let proxy_root = proxy_parent.path().join("proxy");
        let mut proxy =
            Proxy::open_or_clone_in(&remote_url, &proxy_root, DEFAULT_DEPTH).unwrap();

        // Nothing has changed yet.
        assert!(proxy.refresh_branch("master").unwrap().is_empty());

        // Advance the remote: edit one file, add another.
        let remote_git = GitBackend::open(remote_dir.path()).unwrap();
        remote_git
            .commit_upsert("hello.txt", b"CHANGED\n", "edit hello")
            .unwrap();
        remote_git
            .commit_upsert("newfile.txt", b"brand new\n", "add newfile")
            .unwrap();

        // Refresh reports exactly the changed/new paths...
        let mut changed = proxy.refresh_branch("master").unwrap();
        changed.sort();
        assert_eq!(changed, vec!["hello.txt".to_string(), "newfile.txt".to_string()]);

        // ...HEAD fast-forwarded so the projection now serves the new content...
        assert_eq!(proxy.git().read_blob("hello.txt").unwrap(), b"CHANGED\n");
        assert_eq!(proxy.git().read_blob("newfile.txt").unwrap(), b"brand new\n");

        // ...and the proxy is still a shallow copy.
        assert!(proxy.root().join(".git").join("shallow").exists());
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
