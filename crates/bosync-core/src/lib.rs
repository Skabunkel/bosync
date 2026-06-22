//! The bosync sync engine: a [`SyncFilter`] implementation that answers Cloud Filter
//! callbacks out of a [`GitBackend`], plus [`reconcile_path`] which commits local changes
//! back to git.
//!
//! - `fetch_placeholders` projects a git tree directory into placeholders (lazy: only the
//!   folder the user browsed into).
//! - `fetch_data` hydrates a file by streaming the matching git blob.
//! - `delete` lets the OS remove a placeholder; the actual git commit for any local
//!   change (create / modify / delete) is driven by a filesystem watcher in the CLI that
//!   calls [`reconcile_path`]. Cloud Filter's per-file close notifications are unreliable
//!   for detecting edits, so the watcher is the single source of truth for write-back.

use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use cloud_filter::error::{CResult, CloudErrorKind};
use cloud_filter::filter::{info, ticket, Request, SyncFilter};
use cloud_filter::metadata::Metadata;
use cloud_filter::placeholder::{ConvertOptions, Placeholder};
use cloud_filter::placeholder_file::PlaceholderFile;
use cloud_filter::utility::{FileTime, WriteAt};

use bosync_git::GitBackend;

/// Cloud Filter writes must be 4096-aligned except at end-of-file. 64 KiB chunks.
const CHUNK_SIZE: usize = 65536;

/// Windows file attributes indicating the file's data is not local (a dehydrated
/// placeholder). Such a file cannot have been modified, so write-back skips it.
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

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

/// Reconcile a single path under the drive with git, committing if it changed.
///
/// Handles create, modify and delete uniformly:
/// - file missing on disk but present in git  -> commit a removal
/// - file present and different from git       -> commit an add/update
/// - dehydrated placeholder or unchanged file   -> no-op (this is what keeps hydration
///   writes from producing spurious commits, since hydrated content equals git content)
pub fn reconcile_path(git: &GitBackend, root: &Path, abs: &Path) -> anyhow::Result<()> {
    let git_path = match to_git_path(root, abs) {
        Some(p) if !p.is_empty() => p,
        _ => return Ok(()),
    };

    match std::fs::metadata(abs) {
        Ok(meta) => {
            if meta.is_dir() {
                return Ok(()); // directories are implied by their entries
            }
            let attrs = meta.file_attributes();
            if attrs & (FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS) != 0 {
                return Ok(()); // dehydrated placeholder: no local data to commit
            }
            let disk = std::fs::read(abs)?;
            match git.read_blob(&git_path) {
                Ok(existing) if existing == disk => {} // unchanged
                Ok(_) => {
                    git.commit_upsert(&git_path, &disk, &format!("Update {git_path}"))?;
                    tracing::info!(%git_path, "committed update");
                    mark_in_sync_up(root, abs);
                }
                Err(_) => {
                    git.commit_upsert(&git_path, &disk, &format!("Add {git_path}"))?;
                    tracing::info!(%git_path, "committed add");
                    mark_in_sync_up(root, abs);
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

/// Mark a single on-disk file or directory as an in-sync placeholder so Explorer shows it
/// as synced (green check) instead of perpetually "pending". Best-effort.
fn mark_path_in_sync(abs: &Path, is_dir: bool) {
    // Already a placeholder (projected file/dir, or a previously-converted one): just set
    // the in-sync state — idempotent.
    if let Ok(mut ph) = Placeholder::open(abs) {
        if ph.mark_in_sync(true, None).is_ok() {
            return;
        }
        // Opened but not yet a placeholder: convert it (directories need `has_children`).
        let mut options = ConvertOptions::default().mark_in_sync();
        if is_dir {
            options = options.has_children();
        }
        if ph.convert_to_placeholder(options, None).is_ok() {
            return;
        }
    }
    // Full file that can't be opened as a placeholder: convert via a plain file handle.
    if !is_dir {
        if let Ok(file) = std::fs::File::open(abs) {
            let mut ph: Placeholder = file.into();
            let _ = ph.convert_to_placeholder(ConvertOptions::default().mark_in_sync(), None);
        }
    }
}

/// Mark a committed file in-sync, then walk up marking each ancestor directory in-sync (up
/// to and including the sync root). A folder that isn't in-sync makes Explorer show the
/// whole subtree as "pending", so the ancestors matter as much as the file.
fn mark_in_sync_up(root: &Path, abs: &Path) {
    mark_path_in_sync(abs, false);
    let mut current = abs.parent();
    while let Some(dir) = current {
        if !dir.starts_with(root) {
            break;
        }
        mark_path_in_sync(dir, true);
        if dir == root {
            break;
        }
        current = dir.parent();
    }
}

/// Mark a just-renamed item in-sync: the item itself (its whole subtree if it's a folder)
/// and every ancestor directory up to the sync root.
fn mark_renamed_in_sync(root: &Path, abs: &Path) {
    let is_dir = std::fs::metadata(abs).map(|m| m.is_dir()).unwrap_or(false);
    if is_dir {
        mark_tree_in_sync(abs);
    } else {
        mark_path_in_sync(abs, false);
    }
    let mut current = abs.parent();
    while let Some(dir) = current {
        if !dir.starts_with(root) {
            break;
        }
        mark_path_in_sync(dir, true);
        if dir == root {
            break;
        }
        current = dir.parent();
    }
}

/// Recursively mark every existing file and directory under `root` as an in-sync
/// placeholder. Run on mount to clear stale "pending sync" overlays left from a previous
/// session — especially folders, which are created locally and never projected from git.
pub fn mark_tree_in_sync(root: &Path) {
    mark_path_in_sync(root, true);
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            mark_path_in_sync(&path, is_dir);
            if is_dir {
                stack.push(path);
            }
        }
    }
}

pub struct BosyncFilter {
    git: GitBackend,
    /// Absolute path of the sync root (the drive folder).
    root: PathBuf,
}

impl BosyncFilter {
    pub fn new(git: GitBackend, root: PathBuf) -> Self {
        Self { git, root }
    }
}

impl SyncFilter for BosyncFilter {
    fn fetch_data(
        &self,
        request: Request,
        ticket: ticket::FetchData,
        info: info::FetchData,
    ) -> CResult<()> {
        // Resolve from the file's CURRENT path, not the stored routing blob: the drive
        // mirrors the git tree 1:1, so the path is always correct even after a rename (a
        // stale blob would point at a path we've since renamed away and fail to hydrate).
        let git_path = match to_git_path(&self.root, &request.path()) {
            Some(p) if !p.is_empty() => p,
            _ => String::from_utf8_lossy(request.file_blob()).into_owned(),
        };
        let range = info.required_file_range();
        tracing::info!(path = %git_path, ?range, "fetch_data");

        // If the blob is missing (e.g. a stale routing blob after a rename), do NOT return
        // an error: the cloud-filter crate reports fetch failures with an `.unwrap()` that
        // panics and takes the whole provider down. Log and bail out of the read instead.
        let data = match self.git.read_blob(&git_path) {
            Ok(data) => data,
            Err(e) => {
                tracing::warn!(%git_path, "fetch_data: blob not found, skipping: {e:#}");
                return Ok(());
            }
        };

        let end = range.end.min(data.len() as u64);
        let mut position = range.start;
        while position < end {
            let remaining = (end - position) as usize;
            let mut len = remaining.min(CHUNK_SIZE);
            // Keep mid-file writes 4096-aligned; the final write may be unaligned (EOF).
            let unaligned = len % 4096;
            if unaligned != 0 && position + len as u64 != end {
                len -= unaligned;
            }
            let start = position as usize;
            ticket
                .write_at(&data[start..start + len], position)
                .map_err(|_| CloudErrorKind::InvalidRequest)?;
            position += len as u64;
            if position < end {
                let _ = ticket.report_progress(end, position);
            }
        }
        Ok(())
    }

    fn fetch_placeholders(
        &self,
        request: Request,
        ticket: ticket::FetchPlaceholders,
        _info: info::FetchPlaceholders,
    ) -> CResult<()> {
        let absolute = request.path();
        let rel = to_git_path(&self.root, &absolute).ok_or(CloudErrorKind::InvalidRequest)?;
        tracing::info!(dir = %rel, "fetch_placeholders");

        // A folder with no git-projected children (e.g. one the user just created locally)
        // yields an empty placeholder set — a valid, navigable, empty directory.
        let entries = self.git.list_dir(&rel).unwrap_or_default();

        let mut placeholders = Vec::new();
        for e in entries {
            // Full root-relative git path (used as the hydration routing key)...
            let git_path = if rel.is_empty() {
                e.name.clone()
            } else {
                format!("{rel}/{}", e.name)
            };
            // ...but the placeholder NAME must be relative to the directory being
            // populated (just the leaf), or TRANSFER_PLACEHOLDERS rejects it.
            // Skip anything already materialized on disk to avoid duplicates.
            if self.root.join(git_path.replace('/', "\\")).exists() {
                continue;
            }
            // Cloud Filter requires valid timestamps on placeholders; without them
            // enumeration fails with "the cloud operation is invalid".
            let now = FileTime::now();
            let base = if e.is_dir {
                Metadata::directory()
            } else {
                Metadata::file().size(e.size)
            };
            let metadata = base.created(now).written(now).accessed(now);
            placeholders.push(
                PlaceholderFile::new(&e.name)
                    .metadata(metadata)
                    .mark_in_sync()
                    .overwrite()
                    .blob(git_path.into_bytes()),
            );
        }

        ticket
            .pass_with_placeholder(&mut placeholders)
            .map_err(|_| CloudErrorKind::InvalidRequest)?;
        Ok(())
    }

    fn delete(
        &self,
        request: Request,
        ticket: ticket::Delete,
        _info: info::Delete,
    ) -> CResult<()> {
        // Allow the OS to perform the delete; the watcher commits the removal afterwards.
        tracing::debug!(path = ?request.path(), "delete (allowing)");
        ticket.pass().map_err(|_| CloudErrorKind::InvalidRequest)?;
        Ok(())
    }

    fn rename(
        &self,
        request: Request,
        ticket: ticket::Rename,
        info: info::Rename,
    ) -> CResult<()> {
        tracing::info!(src = ?request.path(), dest = ?info.target_path(), "rename (approving)");
        // ONLY approve here. The default filter rejects renames with NOT_SUPPORTED (the
        // 0x8007018B error). We must not touch git yet: Windows may hydrate the file under
        // its OLD path as part of the move, so the old blob has to stay resolvable. The git
        // rename is done in `renamed()`, once the move is complete.
        ticket.pass().map_err(|_| CloudErrorKind::InvalidRequest)?;
        Ok(())
    }

    fn renamed(&self, request: Request, info: info::Renamed) {
        // Now the move is done: `info.source_path()` is the old path, `request.path()` the new.
        let new = request.path();
        let old = info.source_path();
        tracing::info!(?old, ?new, "renamed");

        if let (Some(old_git), Some(new_git)) =
            (to_git_path(&self.root, &old), to_git_path(&self.root, &new))
        {
            if !old_git.is_empty() && !new_git.is_empty() && old_git != new_git {
                match self.git.rename_in_git(&old_git, &new_git) {
                    // `old` was tracked: a proper git rename happened (file or whole folder).
                    Ok(true) => {}
                    // `old` wasn't committed yet (a freshly created file renamed before the
                    // watcher saw it). Commit `new` from disk so git is consistent NOW —
                    // otherwise a refresh would re-project the stale entry as a duplicate.
                    Ok(false) => {
                        if let Ok(content) = std::fs::read(&new) {
                            let _ = self.git.commit_upsert(
                                &new_git,
                                &content,
                                &format!("Add {new_git}"),
                            );
                        }
                    }
                    Err(e) => tracing::warn!(%old_git, %new_git, "git rename failed: {e:#}"),
                }
                // Clear the "pending sync" overlay on the renamed item right away.
                mark_renamed_in_sync(&self.root, &new);
            }
        }
    }

    fn opened(&self, request: Request, _info: info::Opened) {
        tracing::debug!(path = ?request.path(), "opened");
    }

    fn closed(&self, request: Request, _info: info::Closed) {
        tracing::debug!(path = ?request.path(), "closed");
    }

    fn state_changed(&self, changes: Vec<PathBuf>) {
        tracing::debug!(?changes, "state_changed");
    }
}
