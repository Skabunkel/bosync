//! Platform-agnostic write-back: turn a local change under the proxy into a git commit.
//!
//! This is the cross-platform half of the old Cloud Filter engine — it knows nothing about
//! any OS, only about paths, the [`GitBackend`], and the [`CloudSync`] hooks. The Windows
//! Cloud Filter provider (and, later, FUSE / File Provider) drives it through a filesystem
//! watcher.

use std::path::Path;

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

/// Reconcile a single path under the proxy with git, committing if it changed.
///
/// Handles create, modify and delete uniformly:
/// - file missing on disk but present in git  -> commit a removal
/// - file present and different from git       -> commit an add/update
/// - dehydrated placeholder or unchanged file  -> no-op (this is what keeps hydration
///   writes from producing spurious commits, since hydrated content equals git content)
///
/// Generic over [`CloudSync`] so the same logic serves every platform: the "is this a
/// cloud-only placeholder?" question and the post-commit overlay update are the only
/// OS-specific parts, and both go through the trait.
pub fn reconcile_path<C: CloudSync>(
    git: &GitBackend,
    cloud: &C,
    root: &Path,
    abs: &Path,
) -> anyhow::Result<()> {
    let git_path = match to_git_path(root, abs) {
        Some(p) if !p.is_empty() => p,
        _ => return Ok(()),
    };

    match std::fs::metadata(abs) {
        Ok(meta) => {
            if meta.is_dir() {
                return Ok(()); // directories are implied by their entries
            }
            if cloud.is_dehydrated(abs) {
                return Ok(()); // cloud-only placeholder: no local data to commit
            }
            let disk = std::fs::read(abs)?;
            match git.read_blob(&git_path) {
                Ok(existing) if existing == disk => {} // unchanged
                Ok(_) => {
                    git.commit_upsert(&git_path, &disk, &format!("Update {git_path}"))?;
                    tracing::info!(%git_path, "committed update");
                    cloud.mark_in_sync(root, abs);
                }
                Err(_) => {
                    git.commit_upsert(&git_path, &disk, &format!("Add {git_path}"))?;
                    tracing::info!(%git_path, "committed add");
                    cloud.mark_in_sync(root, abs);
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Gone from disk: remove from git if it was tracked.
            if git.read_blob(&git_path).is_ok() {
                git.commit_remove(&git_path, &format!("Delete {git_path}"))?;
                tracing::info!(%git_path, "committed delete");
            }
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
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

        reconcile_path(&git, &NullCloudSync, root, &file).unwrap();

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

        reconcile_path(&git, &NullCloudSync, root, &file).unwrap();

        assert_eq!(before, git.rev_id("HEAD"), "no new commit for unchanged file");
    }

    #[test]
    fn reconcile_commits_a_delete() {
        let (dir, git) = temp_repo();
        let root = dir.path();
        let file = root.join("hello.txt");
        let _ = std::fs::remove_file(&file);

        reconcile_path(&git, &NullCloudSync, root, &file).unwrap();

        assert!(git.read_blob("hello.txt").is_err(), "blob removed from git");
    }
}
