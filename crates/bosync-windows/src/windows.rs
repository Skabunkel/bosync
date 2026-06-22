//! Windows Cloud Filter integration. Only compiled on Windows (see `lib.rs`).

use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use cloud_filter::error::{CResult, CloudErrorKind};
use cloud_filter::filter::{info, ticket, Request, SyncFilter};
use cloud_filter::metadata::Metadata;
use cloud_filter::placeholder::{ConvertOptions, Placeholder};
use cloud_filter::placeholder_file::PlaceholderFile;
use cloud_filter::root::{
    Connection, HydrationType, PopulationType, SecurityId, Session, SyncRootId, SyncRootIdBuilder,
    SyncRootInfo,
};
use cloud_filter::utility::{FileTime, WriteAt};

use bosync_core::{CloudSync, to_git_path};
use bosync_git::GitBackend;

/// Cloud Filter writes must be 4096-aligned except at end-of-file. 64 KiB chunks.
const CHUNK_SIZE: usize = 65536;

/// Windows file attributes indicating the file's data is not local (a dehydrated
/// placeholder). Such a file cannot have been modified, so write-back skips it.
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

// ===========================================================================================
// CloudSync: the platform seam the shared engine drives.
// ===========================================================================================

/// The Windows implementation of [`bosync_core::CloudSync`]. Pairs with [`BosyncFilter`]:
/// the filter answers OS callbacks, this answers the engine's platform questions during
/// write-back and idle maintenance.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsCloudSync;

impl CloudSync for WindowsCloudSync {
    fn is_dehydrated(&self, abs: &Path) -> bool {
        match std::fs::metadata(abs) {
            Ok(meta) => {
                let attrs = meta.file_attributes();
                attrs & (FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS) != 0
            }
            Err(_) => false,
        }
    }

    fn mark_in_sync(&self, root: &Path, abs: &Path) {
        mark_in_sync_up(root, abs);
    }

    fn dehydrate(&self, abs: &Path) {
        // Best-effort: convert to an in-sync placeholder so it can be freed; full Cloud Filter
        // dehydration (CfDehydratePlaceholder) is a later refinement of the idle engine.
        if let Ok(mut ph) = Placeholder::open(abs) {
            let _ = ph.mark_in_sync(true, None);
        }
    }
}

// ===========================================================================================
// In-sync marking helpers (Explorer overlay state).
// ===========================================================================================

/// Mark a single on-disk file or directory as an in-sync placeholder so Explorer shows it
/// as synced (green check) instead of perpetually "pending". Best-effort.
fn mark_path_in_sync(abs: &Path, is_dir: bool) {
    // Already a placeholder (projected file/dir, or a previously-converted one): just set the
    // in-sync state — idempotent.
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

/// Mark a committed file in-sync, then walk up marking each ancestor directory in-sync (up to
/// and including the sync root). A folder that isn't in-sync makes Explorer show the whole
/// subtree as "pending", so the ancestors matter as much as the file.
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

/// Mark a just-renamed item in-sync: the item itself (its whole subtree if it's a folder) and
/// every ancestor directory up to the sync root.
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

/// Recursively mark every existing file and directory under `root` as an in-sync placeholder.
/// Run on mount to clear stale "pending sync" overlays left from a previous session.
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

// ===========================================================================================
// SyncRoot: register + connect the proxy folder with Windows.
// ===========================================================================================

/// A registered sync root. Registration persists in the OS until [`SyncRoot::unregister`].
pub struct SyncRoot {
    id: SyncRootId,
}

impl SyncRoot {
    /// Register `path` as a sync root for the given provider, if not already registered.
    pub fn register(
        provider: &str,
        display_name: &str,
        version: &str,
        path: &Path,
    ) -> Result<Self> {
        let id = SyncRootIdBuilder::new(provider)
            .user_security_id(
                SecurityId::current_user().map_err(|e| anyhow!("current user SID: {e:?}"))?,
            )
            .build();

        let registered = id
            .is_registered()
            .map_err(|e| anyhow!("is_registered: {e:?}"))?;
        if !registered {
            let info = SyncRootInfo::default()
                .with_display_name(display_name)
                .with_hydration_type(HydrationType::Full)
                .with_population_type(PopulationType::Full)
                .with_icon("%SystemRoot%\\system32\\imageres.dll,-1043")
                .with_version(version)
                .with_recycle_bin_uri("http://bosync.local/recyclebin")
                .map_err(|e| anyhow!("recycle bin uri: {e:?}"))?
                .with_path(path)
                .map_err(|e| anyhow!("sync root path {path:?}: {e:?}"))?;
            id.register(info).map_err(|e| anyhow!("register: {e:?}"))?;
            tracing::info!("registered sync root at {path:?}");
        } else {
            tracing::info!("sync root already registered");
        }

        Ok(Self { id })
    }

    /// Build a handle to the (possibly already-registered) sync root without registering.
    pub fn open(provider: &str) -> Result<Self> {
        let id = SyncRootIdBuilder::new(provider)
            .user_security_id(
                SecurityId::current_user().map_err(|e| anyhow!("current user SID: {e:?}"))?,
            )
            .build();
        Ok(Self { id })
    }

    /// Connect a filter session. The returned [`Connection`] stays live until dropped.
    pub fn connect<F: SyncFilter + 'static>(
        &self,
        path: &Path,
        filter: F,
    ) -> Result<Connection<F>> {
        Session::new()
            .connect(path, filter)
            .map_err(|e| anyhow!("connect session: {e:?}"))
    }

    /// Remove the OS registration for this sync root.
    pub fn unregister(&self) -> Result<()> {
        self.id
            .unregister()
            .map_err(|e| anyhow!("unregister: {e:?}"))?;
        tracing::info!("unregistered sync root");
        Ok(())
    }
}

// ===========================================================================================
// BosyncFilter: the Cloud Filter provider (projection + hydration).
// ===========================================================================================

/// The Cloud Filter `SyncFilter` provider. Answers `fetch_placeholders` from the proxy's git
/// tree and `fetch_data` by streaming git blobs. Write-back (commit) is driven separately by
/// the CLI's filesystem watcher via [`bosync_core::reconcile_path`].
pub struct BosyncFilter {
    git: GitBackend,
    /// Absolute path of the sync root (the proxy folder).
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
        // Resolve from the file's CURRENT path, not the stored routing blob: the drive mirrors
        // the git tree 1:1, so the path is always correct even after a rename.
        let git_path = match to_git_path(&self.root, &request.path()) {
            Some(p) if !p.is_empty() => p,
            _ => String::from_utf8_lossy(request.file_blob()).into_owned(),
        };
        let range = info.required_file_range();
        tracing::info!(path = %git_path, ?range, "fetch_data");

        // If the blob is missing (e.g. a stale routing blob after a rename), do NOT return an
        // error: the cloud-filter crate reports fetch failures with an `.unwrap()` that panics
        // and takes the whole provider down. Log and bail out of the read instead.
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
            // ...but the placeholder NAME must be relative to the directory being populated
            // (just the leaf), or TRANSFER_PLACEHOLDERS rejects it.
            // Skip anything already materialized on disk to avoid duplicates.
            if self.root.join(git_path.replace('/', "\\")).exists() {
                continue;
            }
            // Cloud Filter requires valid timestamps on placeholders; without them enumeration
            // fails with "the cloud operation is invalid".
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
        // ONLY approve here. The default filter rejects renames with NOT_SUPPORTED. We must not
        // touch git yet: Windows may hydrate the file under its OLD path as part of the move,
        // so the old blob has to stay resolvable. The git rename is done in `renamed()`.
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
                    // watcher saw it). Commit `new` from disk so git is consistent NOW.
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
