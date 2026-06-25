//! Platform-agnostic write-back: turn a local change under the proxy into a git commit.
//!
//! This is the cross-platform half of the old Cloud Filter engine — it knows nothing about
//! any OS, only about paths, the [`GitBackend`], and the [`CloudSync`] hooks. The Windows
//! Cloud Filter provider (and, later, FUSE / File Provider) drives it through a filesystem
//! watcher.

use std::path::{Path, PathBuf};

use bosync_git::GitBackend;

use crate::mount::CloudSync;

/// Convert an absolute path inside `root` to a `/`-separated git path.
/// Returns `Some("")` for `root` itself, `None` if `abs` is outside `root`.
pub fn to_git_path(root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for c in rel.components() {
        parts.push(c.as_os_str().to_string_lossy().into_owned());
    }
    Some(parts.join("/"))
}

/// What a [`reconcile_path`] call did, so the caller can drive the right state transition.
///
/// Reconcile only owns the git half (committing). Whether a committed file should now show as
/// [`Local`](crate::ProxyState::Local) (push still pending) or [`Synced`](crate::ProxyState::Synced)
/// (nothing to push — the local repo is the source of truth) is a policy decision that belongs
/// to the caller, so reconcile reports the outcome and marks no overlay itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconciled {
    /// A file was added or updated on disk and that change was committed. It is now present
    /// locally with a commit that has not been pushed yet — i.e. [`ProxyState::Local`].
    ///
    /// [`ProxyState::Local`]: crate::ProxyState::Local
    Committed,
    /// A file was removed on disk and the removal was committed.
    Removed,
    /// Nothing to do: unchanged content, a directory, or a cloud-only placeholder.
    Unchanged,
}

/// Reconcile a single path under the proxy with git, committing if it changed.
///
/// Handles create, modify and delete uniformly:
/// - file missing on disk but present in git  -> commit a removal ([`Reconciled::Removed`])
/// - file present and different from git       -> commit an add/update ([`Reconciled::Committed`])
/// - dehydrated placeholder or unchanged file  -> no-op ([`Reconciled::Unchanged`]) (this is what
///   keeps hydration writes from producing spurious commits, since hydrated content equals git
///   content)
///
/// Generic over [`CloudSync`] so the same logic serves every platform: the "is this a
/// cloud-only placeholder?" question is the only OS-specific part, and it goes through the
/// trait. The post-commit overlay transition is left to the caller (see [`Reconciled`]).
pub fn reconcile_path<C: CloudSync>(
    git: &GitBackend,
    cloud: &C,
    root: &Path,
    abs: &Path,
) -> anyhow::Result<Reconciled> {
    let git_path = match to_git_path(root, abs) {
        Some(p) if !p.is_empty() => p,
        _ => return Ok(Reconciled::Unchanged),
    };

    match std::fs::metadata(abs) {
        Ok(meta) => {
            if meta.is_dir() {
                return Ok(Reconciled::Unchanged); // directories are implied by their entries
            }
            if cloud.is_dehydrated(abs) {
                return Ok(Reconciled::Unchanged); // cloud-only placeholder: no local data
            }
            let disk = std::fs::read(abs)?;
            match git.read_blob(&git_path) {
                Ok(existing) if existing == disk => Ok(Reconciled::Unchanged), // unchanged
                Ok(_) => {
                    git.commit_upsert(&git_path, &disk, &format!("Update {git_path}"))?;
                    tracing::info!(%git_path, "committed update");
                    Ok(Reconciled::Committed)
                }
                Err(_) => {
                    git.commit_upsert(&git_path, &disk, &format!("Add {git_path}"))?;
                    tracing::info!(%git_path, "committed add");
                    Ok(Reconciled::Committed)
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Gone from disk: remove from git if it was tracked.
            if git.read_blob(&git_path).is_ok() {
                git.commit_remove(&git_path, &format!("Delete {git_path}"))?;
                tracing::info!(%git_path, "committed delete");
                Ok(Reconciled::Removed)
            } else {
                Ok(Reconciled::Unchanged)
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// Flush all outstanding local changes to the remote — the shutdown path, so batched-but-unpushed
/// work is never lost when the mount exits.
///
/// First reconciles every still-`pending` path into a commit (a reconcile error for one path is
/// logged and skipped, never aborting the flush). Then, if the proxy's `HEAD` has moved past
/// `pushed_head`, pushes once. Returns:
///
/// - `Ok(Some(head))` — a push happened; `head` is the newly-published commit (the caller can mark
///   the tree in-sync and remember it as the new `pushed_head`).
/// - `Ok(None)` — nothing was outstanding, so no push was needed.
/// - `Err(_)` — the push itself failed (e.g. a network remote with no pack send). The reconciled
///   commits are still durable in the local `.git`, so this is "not yet on the remote", not data
///   loss; the caller should surface it but the bytes are safe.
pub fn flush_to_remote<C: CloudSync>(
    git: &GitBackend,
    cloud: &C,
    root: &Path,
    remote: &str,
    pending: &[PathBuf],
    pushed_head: Option<&str>,
) -> anyhow::Result<Option<String>> {
    for path in pending {
        if let Err(e) = reconcile_path(git, cloud, root, path) {
            tracing::warn!(?path, "reconcile during flush failed: {e:#}");
        }
    }
    let head = git.rev_id("HEAD");
    if head.as_deref() != pushed_head {
        git.push(remote)?;
        tracing::info!("flush: pushed outstanding changes to remote");
        return Ok(head);
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mount::NullCloudSync;
    use crate::testutil::TmpDir;

    fn temp_repo() -> (TmpDir, GitBackend) {
        let dir = TmpDir::new();
        let git = GitBackend::init_sample(dir.path()).expect("init sample");
        (dir, git)
    }

    #[test]
    fn to_git_path_maps_relative_and_rejects_outside() {
        let root = Path::new("/srv/drive");
        assert_eq!(to_git_path(root, Path::new("/srv/drive")).as_deref(), Some(""));
        assert_eq!(
            to_git_path(root, Path::new("/srv/drive/docs/guide.md")).as_deref(),
            Some("docs/guide.md")
        );
        assert_eq!(to_git_path(root, Path::new("/elsewhere/x")), None);
    }

    #[test]
    fn reconcile_commits_a_local_edit() {
        let (dir, git) = temp_repo();
        let root = dir.path();
        let file = root.join("hello.txt");
        std::fs::write(&file, b"changed!\n").unwrap();

        let outcome = reconcile_path(&git, &NullCloudSync, root, &file).unwrap();

        assert_eq!(outcome, Reconciled::Committed);
        assert_eq!(git.read_blob("hello.txt").unwrap(), b"changed!\n");
    }

    #[test]
    fn reconcile_is_noop_when_unchanged() {
        let (dir, git) = temp_repo();
        let root = dir.path();
        let before = git.rev_id("HEAD");
        let file = root.join("hello.txt");
        // Write back the exact committed content.
        std::fs::write(&file, git.read_blob("hello.txt").unwrap()).unwrap();

        let outcome = reconcile_path(&git, &NullCloudSync, root, &file).unwrap();

        assert_eq!(outcome, Reconciled::Unchanged);
        assert_eq!(before, git.rev_id("HEAD"), "no new commit for unchanged file");
    }

    #[test]
    fn reconcile_commits_a_delete() {
        let (dir, git) = temp_repo();
        let root = dir.path();
        let file = root.join("hello.txt");
        let _ = std::fs::remove_file(&file);

        let outcome = reconcile_path(&git, &NullCloudSync, root, &file).unwrap();

        assert_eq!(outcome, Reconciled::Removed);
        assert!(git.read_blob("hello.txt").is_err(), "blob removed from git");
    }

    #[test]
    fn flush_reconciles_uncommitted_work_and_pushes_it() {
        // A remote (the push target) and a shallow clone of it (the proxy/working copy).
        let remote_dir = TmpDir::new();
        GitBackend::init_sample(remote_dir.path()).expect("init remote");
        let remote_url = remote_dir.path().to_string_lossy().into_owned();

        let proxy_dir = TmpDir::new();
        let proxy = GitBackend::clone_shallow(&remote_url, proxy_dir.path(), 1).expect("clone");
        let root = proxy_dir.path();

        // Simulate the shutdown race: a file written to the working copy that the debounce never
        // got to reconcile (so HEAD still equals what the remote has).
        let pushed_head = proxy.rev_id("HEAD");
        let file = root.join("urgent.txt");
        std::fs::write(&file, b"must not be lost\n").unwrap();

        let result = flush_to_remote(
            &proxy,
            &NullCloudSync,
            root,
            &remote_url,
            &[file],
            pushed_head.as_deref(),
        )
        .expect("flush");

        // The flush committed the file and pushed — HEAD advanced and a new head was returned.
        assert!(result.is_some(), "flush should report a push");
        assert_ne!(proxy.rev_id("HEAD"), pushed_head, "flush committed the file");

        // And it actually landed on the remote.
        let remote = GitBackend::open(remote_dir.path()).expect("open remote");
        assert_eq!(remote.read_blob("urgent.txt").unwrap(), b"must not be lost\n");
    }

    #[test]
    fn flush_is_a_noop_when_nothing_is_outstanding() {
        let remote_dir = TmpDir::new();
        GitBackend::init_sample(remote_dir.path()).expect("init remote");
        let remote_url = remote_dir.path().to_string_lossy().into_owned();

        let proxy_dir = TmpDir::new();
        let proxy = GitBackend::clone_shallow(&remote_url, proxy_dir.path(), 1).expect("clone");

        let pushed_head = proxy.rev_id("HEAD");
        let result =
            flush_to_remote(&proxy, &NullCloudSync, proxy_dir.path(), &remote_url, &[], pushed_head.as_deref())
                .expect("flush");

        assert!(result.is_none(), "nothing outstanding → no push");
    }
}
