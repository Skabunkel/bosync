//! Git backend for bosync.
//!
//! Wraps a [`gix::ThreadSafeRepository`] and exposes the handful of operations the
//! Cloud Filter callbacks need: list a directory of a commit tree, read a blob, and
//! commit changes back. The repository is the "cloud": placeholders are projected
//! from `HEAD` and hydrated from blobs.
//!
//! All public methods are `&self` and take a fresh thread-local `Repository` per call
//! (via [`gix::ThreadSafeRepository::to_thread_local`]) so the backend is `Sync` and can
//! be shared across the Cloud Filter callback threads.

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{anyhow, bail, Context, Result};
use gix::object::tree::EntryKind;

/// A list of blobs as `(path_relative_to_some_root, content)` pairs.
type BlobList = Vec<(String, Vec<u8>)>;

/// One entry of a directory listing taken from a git tree.
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// Leaf name (no path separators).
    pub name: String,
    pub is_dir: bool,
    /// Size of the blob in bytes (0 for directories).
    pub size: u64,
}

/// A single change to apply to the tree in one commit.
pub enum TreeOp {
    /// Create or overwrite a file at this `/`-separated path with these bytes.
    Upsert(String, Vec<u8>),
    /// Remove the file/dir at this `/`-separated path.
    Remove(String),
}

/// Thread-safe handle to the backing git repository.
#[derive(Clone)]
pub struct GitBackend {
    repo: gix::ThreadSafeRepository,
}

impl GitBackend {
    /// Open an existing repository at `path`.
    pub fn open(path: &Path) -> Result<Self> {
        let repo = gix::open(path).with_context(|| format!("opening git repo at {path:?}"))?;
        Ok(Self {
            repo: repo.into_sync(),
        })
    }

    /// List the entries of the tree at the given root-relative `/`-separated path.
    /// An empty path lists the root of the `HEAD` tree.
    pub fn list_dir(&self, rel: &str) -> Result<Vec<DirEntry>> {
        let repo = self.repo.to_thread_local();
        let tree = if rel.is_empty() {
            // Unborn branch (freshly `git init`ed repo, no commits): present an empty drive.
            match repo.head_commit() {
                Ok(commit) => commit.tree()?,
                Err(_) => return Ok(Vec::new()),
            }
        } else {
            // A path not present in the current tree (e.g. a folder the user just created
            // locally, or anything in an unborn repo) has no git-projected children. Return
            // an empty listing rather than erroring, so the folder stays navigable.
            let spec = format!("HEAD:{rel}");
            let tree = repo
                .rev_parse_single(spec.as_str())
                .ok()
                .and_then(|id| id.object().ok())
                .and_then(|obj| obj.try_into_tree().ok());
            match tree {
                Some(tree) => tree,
                None => return Ok(Vec::new()),
            }
        };

        let decoded = tree.decode()?;
        let mut out = Vec::with_capacity(decoded.entries.len());
        for entry in &decoded.entries {
            let is_dir = entry.mode.is_tree();
            let size = if is_dir {
                0
            } else {
                repo.find_object(entry.oid)?.data.len() as u64
            };
            out.push(DirEntry {
                name: entry.filename.to_string(),
                is_dir,
                size,
            });
        }
        Ok(out)
    }

    /// Stat a root-relative path in `HEAD`: `Some((is_dir, size))`, or `None` if absent.
    /// The empty path is the root tree. Cheap — resolves the object header, never the data
    /// for directories; used by the FUSE projection for `lookup`/`getattr`.
    pub fn stat(&self, rel: &str) -> Option<(bool, u64)> {
        if rel.is_empty() {
            return Some((true, 0));
        }
        let repo = self.repo.to_thread_local();
        let id = repo
            .rev_parse_single(format!("HEAD:{rel}").as_str())
            .ok()?;
        let obj = id.object().ok()?;
        match obj.kind {
            gix::object::Kind::Tree => Some((true, 0)),
            gix::object::Kind::Blob => Some((false, obj.data.len() as u64)),
            _ => None,
        }
    }

    /// Read the full contents of the blob at the given root-relative path in `HEAD`.
    pub fn read_blob(&self, rel: &str) -> Result<Vec<u8>> {
        let repo = self.repo.to_thread_local();
        let spec = format!("HEAD:{rel}");
        let id = repo
            .rev_parse_single(spec.as_str())
            .with_context(|| format!("resolving blob path {rel:?}"))?;
        let obj = id.object()?;
        Ok(obj.data.clone())
    }

    /// Apply `ops` as a single commit on `HEAD`. Creates an initial commit if the
    /// repository is unborn (no `HEAD` yet).
    ///
    /// For a repository with a working tree, this also mirrors the change into the working
    /// directory and resets the index to the new commit, so the backing folder shows the
    /// files and `git status` stays clean — i.e. it behaves like a normal `git` commit, not
    /// a detached object-store write (which is what left the repo looking "broken").
    pub fn commit(&self, ops: Vec<TreeOp>, message: &str) -> Result<()> {
        let repo = self.repo.to_thread_local();
        let workdir = repo.workdir().map(|p| p.to_owned());

        // Base tree: HEAD's tree, or the empty tree for an unborn branch.
        let parent = repo.head_commit().ok();
        let base_tree = match &parent {
            Some(commit) => commit.tree_id()?.detach(),
            None => gix::ObjectId::empty_tree(repo.object_hash()),
        };

        let mut editor = repo.edit_tree(base_tree)?;
        for op in &ops {
            match op {
                TreeOp::Upsert(path, data) => {
                    let oid = repo.write_blob(data)?.detach();
                    editor.upsert(path.as_str(), EntryKind::Blob, oid)?;
                    if let Some(wd) = &workdir {
                        let full = join_rel(wd, path);
                        if let Some(parent) = full.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::write(&full, data)?;
                    }
                }
                TreeOp::Remove(path) => {
                    editor.remove(path.as_str())?;
                    if let Some(wd) = &workdir {
                        let full = join_rel(wd, path);
                        // `path` may be a whole subtree (folder rename) or a single file.
                        if full.is_dir() {
                            let _ = std::fs::remove_dir_all(&full);
                        } else {
                            let _ = std::fs::remove_file(&full);
                        }
                        // Prune now-empty parent dirs so the backing repo's status stays clean.
                        prune_empty_dirs(wd, full.parent());
                    }
                }
            }
        }
        let new_tree = editor.write()?.detach();

        let parents: Vec<gix::ObjectId> = parent.iter().map(|c| c.id().detach()).collect();
        let id = repo.commit("HEAD", message, new_tree, parents)?;

        // Reset the index to the new commit so the working tree we just wrote matches it.
        if workdir.is_some() {
            if let Ok(mut index) = repo.index_from_tree(&new_tree) {
                let _ = index.write(gix::index::write::Options::default());
            }
        }

        tracing::info!(commit = %id.detach(), "committed: {message}");
        Ok(())
    }

    /// Convenience: commit a single upsert.
    pub fn commit_upsert(&self, rel: &str, data: &[u8], message: &str) -> Result<()> {
        self.commit(vec![TreeOp::Upsert(rel.to_owned(), data.to_vec())], message)
    }

    /// Convenience: commit a single removal.
    pub fn commit_remove(&self, rel: &str, message: &str) -> Result<()> {
        self.commit(vec![TreeOp::Remove(rel.to_owned())], message)
    }

    /// Record a rename `old` -> `new` as one commit, moving the committed content. Reads
    /// content from git (not disk), so it's correct even for an online-only placeholder.
    ///
    /// Handles both files and whole directories: if `old` is a tree, every descendant file
    /// is moved from `old/<rel>` to `new/<rel>` (this is what stops a folder rename from
    /// collapsing into a single file). Returns `false` if `old` isn't tracked at all.
    pub fn rename_in_git(&self, old: &str, new: &str) -> Result<bool> {
        let blobs = match self.collect_blobs_under("HEAD", old)? {
            Some(blobs) => blobs,
            None => return Ok(false),
        };
        let mut ops = vec![TreeOp::Remove(old.to_owned())];
        for (rel, content) in blobs {
            let target = if rel.is_empty() {
                new.to_owned()
            } else {
                format!("{new}/{rel}")
            };
            ops.push(TreeOp::Upsert(target, content));
        }
        self.commit(ops, &format!("Rename {old} -> {new}"))?;
        Ok(true)
    }

    /// Collect every blob at or under `path` as `(rel_path_under_path, content)`.
    /// For a single file `rel_path` is empty. Returns `None` if `path` isn't in `rev`.
    fn collect_blobs_under(&self, rev: &str, path: &str) -> Result<Option<BlobList>> {
        let repo = self.repo.to_thread_local();
        let id = match repo.rev_parse_single(format!("{rev}:{path}").as_str()) {
            Ok(id) => id,
            Err(_) => return Ok(None),
        };
        let obj = id.object()?;
        match obj.kind {
            gix::object::Kind::Blob => Ok(Some(vec![(String::new(), obj.data.clone())])),
            gix::object::Kind::Tree => {
                let tree = obj.try_into_tree()?;
                let mut out = Vec::new();
                collect_tree_blobs(&repo, &tree, "", &mut out)?;
                Ok(Some(out))
            }
            _ => Ok(None),
        }
    }

    /// Create a fresh repository at `path` with a few sample files, for demos.
    pub fn init_sample(path: &Path) -> Result<Self> {
        let repo = gix::init(path).with_context(|| format!("git init at {path:?}"))?;
        let backend = Self {
            repo: repo.into_sync(),
        };
        backend.commit(
            vec![
                TreeOp::Upsert(
                    "README.md".into(),
                    b"# bosync sample repo\n\nThis repo is the \"cloud\" backing a bosync mount.\n"
                        .to_vec(),
                ),
                TreeOp::Upsert(
                    "hello.txt".into(),
                    b"Hello from a git blob! Open me to trigger hydration.\n".to_vec(),
                ),
                TreeOp::Upsert(
                    "docs/guide.md".into(),
                    b"## Guide\n\nBrowsing this folder lazily projects the `docs` tree into placeholders.\n"
                        .to_vec(),
                ),
            ],
            "Initial sample commit",
        )?;
        Ok(backend)
    }

    // ---- Shallow-clone proxy primitives --------------------------------------------

    /// Shallow-clone `url` into `dest` at the given `depth` and check out its working tree.
    ///
    /// `url` may be `ssh://` / `git@host:owner/repo` (uses the OS ssh program) or
    /// `http(s)://` (pure-Rust rustls transport). This is step 1 of the plan: the checked-out
    /// `dest` folder becomes the on-disk proxy the OS projects as a virtual drive.
    pub fn clone_shallow(url: &str, dest: &Path, depth: u32) -> Result<Self> {
        use gix::remote::fetch::Shallow;

        let depth = NonZeroU32::new(depth.max(1)).expect("depth >= 1");
        let should_interrupt = AtomicBool::new(false);

        let mut fetch = gix::prepare_clone(url, dest)
            .with_context(|| format!("preparing shallow clone of {url} into {dest:?}"))?
            .with_shallow(Shallow::DepthAtRemote(depth));
        let (mut checkout, _) = fetch
            .fetch_then_checkout(gix::progress::Discard, &should_interrupt)
            .with_context(|| format!("fetching {url}"))?;
        let (repo, _) = checkout
            .main_worktree(gix::progress::Discard, &should_interrupt)
            .with_context(|| format!("checking out worktree at {dest:?}"))?;

        tracing::info!(%url, ?dest, depth = depth.get(), "shallow clone complete");
        Ok(Self {
            repo: repo.into_sync(),
        })
    }

    /// Refresh the proxy by redoing the shallow copy from `url`, as the plan describes.
    ///
    /// Re-cloning is the simplest way to get a clean, current proxy; local commits are pushed
    /// on save *before* any refresh, so nothing unpushed is lost here. Returns a fresh backend
    /// — callers sharing the old handle (the platform provider) must re-acquire it.
    pub fn refresh_shallow(url: &str, dest: &Path, depth: u32) -> Result<Self> {
        if dest.exists() {
            std::fs::remove_dir_all(dest)
                .with_context(|| format!("clearing proxy {dest:?} before refresh"))?;
        }
        Self::clone_shallow(url, dest, depth)
    }

    /// Fetch the latest of `branch` from `url` at `depth`, staying shallow, and fast-forward the
    /// checked-out branch to it — the gitoxide equivalent of `git fetch --depth=1 origin <branch>`
    /// followed by advancing the local branch to the fetched tip.
    ///
    /// The update is done **in place** (it never wipes the proxy): it adds the new objects, moves
    /// the `refs/remotes/origin/<branch>` tracking ref, then re-points `HEAD`'s branch at the new
    /// commit. A live mount reads `HEAD` afresh on every callback, so it immediately projects the
    /// new tree without re-acquiring the git handle — which is what makes background refresh safe
    /// while the drive is mounted. Returns `true` if `HEAD` moved.
    ///
    /// `with_shallow(DepthAtRemote)` keeps the history clamped to `depth` so the fetch never
    /// deepens the proxy. (There is no pure-Rust `gc`/`reflog expire` in gix yet, so loose
    /// objects from previous fetches are not reclaimed — a known size trade-off, see
    /// [`Self::prune`].)
    pub fn fetch_shallow(&self, url: &str, branch: &str, depth: u32) -> Result<bool> {
        use gix::refs::transaction::PreviousValue;
        use gix::remote::{fetch::Shallow, Direction};

        let depth = NonZeroU32::new(depth.max(1)).expect("depth >= 1");
        let should_interrupt = AtomicBool::new(false);
        let repo = self.repo.to_thread_local();

        let track = format!("refs/remotes/origin/{branch}");
        let tracked_oid = |repo: &gix::Repository| -> Option<gix::ObjectId> {
            repo.try_find_reference(track.as_str())
                .ok()
                .flatten()
                .and_then(|r| r.try_id().map(|id| id.detach()))
        };
        // Compare the *remote* tracking ref, not local HEAD: a local (un-pushed) commit moves
        // HEAD but not the remote, and must not count as the remote having advanced.
        let remote_before = tracked_oid(&repo);

        // `+refs/heads/<branch>:refs/remotes/origin/<branch>` — fetch just the one branch and
        // force-update its tracking ref, exactly like `git fetch origin <branch>`.
        let refspec = format!("+refs/heads/{branch}:{track}");
        // Prefer the `origin` remote configured at clone time: gix stored and parsed its URL
        // itself, so it round-trips correctly (a hand-built `file://C:/…` URL does not). Fall
        // back to the caller's URL for proxies without an `origin`.
        let remote = match repo.find_remote("origin") {
            Ok(remote) => remote,
            Err(_) => repo
                .remote_at(url)
                .with_context(|| format!("addressing remote {url}"))?,
        };
        let remote = remote
            .with_refspecs(Some(refspec.as_bytes()), Direction::Fetch)
            .context("setting fetch refspec")?;

        remote
            .connect(Direction::Fetch)
            .with_context(|| format!("connecting to {url}"))?
            .prepare_fetch(gix::progress::Discard, Default::default())
            .context("preparing fetch")?
            .with_shallow(Shallow::DepthAtRemote(depth))
            .receive(gix::progress::Discard, &should_interrupt)
            .with_context(|| format!("fetching {branch} from {url}"))?;

        let remote_after = tracked_oid(&repo);
        let moved = remote_before != remote_after;

        // Only when the remote genuinely advanced: fast-forward the checked-out branch to it, so
        // the projection (which reads `HEAD`) reflects the change. Without push there is nothing
        // to lose by force-moving it — a moved remote means someone else advanced the branch.
        if moved {
            if let (Some(tip), Ok(Some(branch_ref))) = (remote_after, repo.head_name()) {
                repo.reference(
                    branch_ref.as_bstr(),
                    tip,
                    PreviousValue::Any,
                    "bosync: fast-forward to fetched tip",
                )
                .context("fast-forwarding HEAD to fetched tip")?;
            }
        }

        tracing::info!(%url, %branch, moved, "shallow fetch complete");
        Ok(moved)
    }

    /// Push the proxy's `HEAD` back to `url`.
    ///
    /// gix 0.84 can't send packs over the network, so a *network* push (ssh/https) still isn't
    /// possible — that call errors and the caller keeps the commit locally. But a **local**
    /// target (`file://` or a filesystem path — which is exactly what a same-machine remote is)
    /// can be pushed in pure Rust: we copy the new object closure into the target's object store
    /// and fast-forward its branch, then mirror the tree into its working copy so `git log` and
    /// the folder both show the change.
    pub fn push(&self, url: &str) -> Result<()> {
        use gix::refs::transaction::PreviousValue;

        let Some(target_path) = local_remote_path(url) else {
            bail!("push to non-local remote {url} not supported (gix 0.84 has no pack send)");
        };

        let proxy = self.repo.to_thread_local();
        let head_id = match proxy.head_commit() {
            Ok(c) => c.id().detach(),
            Err(_) => return Ok(()), // nothing committed yet
        };
        let branch = proxy
            .head_name()
            .ok()
            .flatten()
            .ok_or_else(|| anyhow!("proxy HEAD is detached; cannot push"))?;

        let target = gix::open(&target_path)
            .with_context(|| format!("opening local target repo at {target_path:?}"))?;

        // Already at our commit? Nothing to do.
        let target_tip = target
            .try_find_reference(branch.as_bstr())
            .ok()
            .flatten()
            .and_then(|r| r.try_id().map(|id| id.detach()));
        if target_tip == Some(head_id) {
            return Ok(());
        }

        // Copy the objects the target is missing (the new commit, its new trees and blobs). The
        // walk stops as soon as it meets an object the target already has — and since the proxy
        // is a shallow clone of this target, the unchanged history/subtrees are all already there.
        copy_missing_objects(&proxy, &target, head_id)?;

        // Fast-forward the target's branch to the pushed commit.
        target
            .reference(branch.as_bstr(), head_id, PreviousValue::Any, "bosync: push")
            .context("updating target branch ref")?;

        // Mirror the new tree into the target's working copy + index (skip a bare repo).
        if target.workdir().is_some() {
            let tree_id = target.find_object(head_id)?.try_into_commit()?.tree_id()?.detach();
            materialize_worktree(&target, &tree_id)?;
        }

        tracing::info!(%url, commit = %head_id, target = ?target_path, "pushed to local target");
        Ok(())
    }

    /// Prune to keep the proxy's git size down (idle maintenance).
    ///
    /// A shallow clone already carries almost no history; gix exposes no gc yet, so this is a
    /// documented no-op rather than shelling out to a non-Rust tool. The seam exists so the
    /// idle engine can call it today and gain real compaction when gix grows the capability.
    pub fn prune(&self) -> Result<()> {
        tracing::debug!("prune: no-op (gix has no gc; shallow clone already minimal)");
        Ok(())
    }

    // ---- Sync interview primitives -------------------------------------------------

    /// The commit id `rev` resolves to (default `HEAD`), or `None` for an unborn branch.
    pub fn rev_id(&self, rev: &str) -> Option<String> {
        let repo = self.repo.to_thread_local();
        repo.rev_parse_single(rev).ok().map(|id| id.detach().to_string())
    }

    /// Compute the git blob object id (hex) for `data` WITHOUT writing it to the repo.
    /// Lets a client hash its local file and ask whether that version is current.
    pub fn hash_blob(&self, data: &[u8]) -> Result<String> {
        let kind = self.repo.to_thread_local().object_hash();
        let id = gix::objs::compute_hash(kind, gix::object::Kind::Blob, data)
            .context("hashing blob")?;
        Ok(id.to_string())
    }

    /// Read an object's bytes by hex id, if present in the object database. Used to fetch
    /// a client's *base* version (X) so the server can diff it against the current one (Y).
    pub fn read_oid(&self, oid_hex: &str) -> Option<Vec<u8>> {
        let oid = gix::ObjectId::from_hex(oid_hex.as_bytes()).ok()?;
        let repo = self.repo.to_thread_local();
        repo.find_object(oid).ok().map(|obj| obj.data.clone())
    }

    /// The blob id (hex) of `path` at `rev`, or `None` if the path is absent there.
    /// Cheap: walks tree objects along the path, never reads blob contents.
    pub fn path_oid(&self, rev: &str, path: &str) -> Option<String> {
        let repo = self.repo.to_thread_local();
        let spec = format!("{rev}:{path}");
        repo.rev_parse_single(spec.as_str())
            .ok()
            .map(|id| id.detach().to_string())
    }

    /// Recursively list every file at `rev` as `(path, blob_oid_hex, size)`.
    /// Returns an empty list for an unborn branch.
    pub fn walk(&self, rev: &str) -> Result<Vec<(String, String, u64)>> {
        let repo = self.repo.to_thread_local();
        let tree = match repo.rev_parse_single(rev) {
            Ok(id) => {
                let obj = id.object()?;
                match obj.kind {
                    gix::object::Kind::Commit => obj.try_into_commit()?.tree()?,
                    gix::object::Kind::Tree => obj.try_into_tree()?,
                    _ => return Ok(Vec::new()),
                }
            }
            Err(_) => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        walk_tree(&repo, &tree, "", &mut out)?;
        Ok(out)
    }
}

/// Normalize a remote URL for gix on every platform. A **local** remote (`file://…` or a bare
/// path) becomes a plain forward-slash path — gix's clone/fetch mishandle `file://C:\…` URLs on
/// Windows, but a plain path works. A **network** remote is returned untouched.
pub fn normalize_remote(url: &str) -> String {
    match local_remote_path(url) {
        Some(p) => p.to_string_lossy().replace('\\', "/"),
        None => url.trim().to_string(),
    }
}

/// Resolve a remote `url` to a local filesystem path if it is one (`file://…` or a bare path),
/// or `None` for a network remote (`ssh://`, `http(s)://`, `git@host:owner/repo`) which gix
/// can't push to.
fn local_remote_path(url: &str) -> Option<PathBuf> {
    let u = url.trim();
    if let Some(rest) = u.strip_prefix("file://") {
        // `file:///C:/x` → `/C:/x` → `C:/x`; `file://C:\x` → `C:\x`; `file:///home/x` → `/home/x`.
        let rest = match rest.strip_prefix('/') {
            Some(after) if is_windows_drive(after) => after,
            _ => rest,
        };
        return Some(PathBuf::from(rest));
    }
    // Any explicit scheme, or an scp-like `host:path`, is a network remote.
    if u.contains("://") || is_scp_like(u) {
        return None;
    }
    Some(PathBuf::from(u))
}

/// Whether `s` begins with a Windows drive spec like `C:` (so a leading `/` from a `file:///`
/// URL should be dropped).
fn is_windows_drive(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Whether `s` is an scp-like remote (`git@host:owner/repo`, `host.tld:path`) rather than a
/// local path. A Windows `C:\path` is *not* scp-like (single-letter host, no `@`/`.`).
fn is_scp_like(s: &str) -> bool {
    if s.contains("://") {
        return false;
    }
    match s.split_once(':') {
        Some((before, _)) => before.contains('@') || (before.len() > 1 && before.contains('.')),
        None => false,
    }
}

/// Copy every object reachable from `start` that `dst` is missing, from `src` into `dst`,
/// preserving object ids (content-addressed: writing the same bytes reproduces the same oid).
/// Stops descending as soon as an object is already present in `dst`.
fn copy_missing_objects(
    src: &gix::Repository,
    dst: &gix::Repository,
    start: gix::ObjectId,
) -> Result<()> {
    use gix::objs::{CommitRef, Kind, TreeRef};
    use gix::prelude::Write;

    let hash = src.object_hash();
    let mut stack = vec![start];
    while let Some(id) = stack.pop() {
        if dst.has_object(id) {
            continue;
        }
        let obj = src
            .find_object(id)
            .with_context(|| format!("reading object {id} to copy"))?;
        let kind = obj.kind;
        let data = obj.data.clone();
        match kind {
            Kind::Commit => {
                let commit = CommitRef::from_bytes(&data, hash)?;
                stack.push(commit.tree());
                stack.extend(commit.parents());
            }
            Kind::Tree => {
                let tree = TreeRef::from_bytes(&data, hash)
                    .map_err(|e| anyhow!("decoding tree {id}: {e}"))?;
                for entry in &tree.entries {
                    stack.push(entry.oid.to_owned());
                }
            }
            Kind::Blob | Kind::Tag => {}
        }
        dst.write_buf(kind, &data)
            .map_err(|e| anyhow!("writing object {id} into target: {e}"))?;
    }
    Ok(())
}

/// Write every blob of `tree_id` into `repo`'s working directory and reset its index to match,
/// so the working copy and `git status` reflect the tree (used after a local push).
fn materialize_worktree(repo: &gix::Repository, tree_id: &gix::ObjectId) -> Result<()> {
    let Some(wd) = repo.workdir().map(|p| p.to_owned()) else {
        return Ok(());
    };
    if let Ok(mut index) = repo.index_from_tree(tree_id) {
        let _ = index.write(gix::index::write::Options::default());
    }
    let tree = repo.find_object(*tree_id)?.try_into_tree()?;
    let mut blobs = Vec::new();
    collect_tree_blobs(repo, &tree, "", &mut blobs)?;
    for (rel, content) in blobs {
        let full = join_rel(&wd, &rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&full, content)?;
    }
    Ok(())
}

/// Remove empty directories from `dir` upward, stopping at `base` or the first non-empty
/// directory (`remove_dir` only succeeds on empty ones).
fn prune_empty_dirs(base: &Path, mut dir: Option<&Path>) {
    while let Some(d) = dir {
        if d == base || !d.starts_with(base) {
            break;
        }
        if std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

/// Join a `/`-separated repo-relative path onto a base directory, component by component
/// (so it's correct regardless of the platform path separator).
fn join_rel(base: &Path, rel: &str) -> std::path::PathBuf {
    let mut full = base.to_path_buf();
    for part in rel.split('/') {
        full.push(part);
    }
    full
}

fn collect_tree_blobs(
    repo: &gix::Repository,
    tree: &gix::Tree<'_>,
    prefix: &str,
    out: &mut Vec<(String, Vec<u8>)>,
) -> Result<()> {
    for entry in &tree.decode()?.entries {
        let name = entry.filename.to_string();
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if entry.mode.is_tree() {
            let sub = repo.find_object(entry.oid)?.try_into_tree()?;
            collect_tree_blobs(repo, &sub, &rel, out)?;
        } else {
            let content = repo.find_object(entry.oid)?.data.clone();
            out.push((rel, content));
        }
    }
    Ok(())
}

fn walk_tree(
    repo: &gix::Repository,
    tree: &gix::Tree<'_>,
    prefix: &str,
    out: &mut Vec<(String, String, u64)>,
) -> Result<()> {
    let decoded = tree.decode()?;
    for entry in &decoded.entries {
        let name = entry.filename.to_string();
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if entry.mode.is_tree() {
            let sub = repo.find_object(entry.oid)?.try_into_tree()?;
            walk_tree(repo, &sub, &path, out)?;
        } else {
            // Header lookup gives the size without decompressing the whole blob.
            let size = repo.find_header(entry.oid)?.size();
            out.push((path, entry.oid.to_string(), size));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A unique temp directory (no external crate, to keep deps light).
    fn tmp(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("bosync-git-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn local_remote_path_classifies_urls() {
        // Network remotes → not pushable locally.
        assert!(local_remote_path("ssh://git@host/o/r").is_none());
        assert!(local_remote_path("https://github.com/o/r.git").is_none());
        assert!(local_remote_path("git@github.com:o/r.git").is_none());
        // Local forms → a path.
        assert_eq!(local_remote_path("file:///C:/x").unwrap(), PathBuf::from("C:/x"));
        assert_eq!(local_remote_path(r"file://C:\x").unwrap(), PathBuf::from(r"C:\x"));
        assert_eq!(local_remote_path(r"C:\repo").unwrap(), PathBuf::from(r"C:\repo"));
        assert_eq!(
            local_remote_path("/home/me/repo").unwrap(),
            PathBuf::from("/home/me/repo")
        );
    }

    #[test]
    fn push_to_local_target_publishes_commit_and_worktree() {
        // A target repo with a working tree stands in for a same-machine remote.
        let target_dir = tmp("target");
        GitBackend::init_sample(&target_dir).unwrap();
        let url = target_dir.to_string_lossy().replace('\\', "/");

        // Shallow-clone it into a proxy, commit a new file there, and push it back.
        let proxy_dir = tmp("proxy").join("p");
        let proxy = GitBackend::clone_shallow(&url, &proxy_dir, 1).unwrap();
        proxy
            .commit_upsert("pushed.txt", b"hi from proxy\n", "Add pushed.txt")
            .unwrap();
        proxy.push(&url).unwrap();

        // The target has the object, its branch advanced, and the file is in its working tree.
        let target = GitBackend::open(&target_dir).unwrap();
        assert_eq!(target.read_blob("pushed.txt").unwrap(), b"hi from proxy\n");
        assert!(target_dir.join("pushed.txt").exists());
        assert_eq!(proxy.rev_id("HEAD"), target.rev_id("HEAD"));

        // Pushing again is a clean no-op (already up to date).
        proxy.push(&url).unwrap();

        let _ = std::fs::remove_dir_all(&target_dir);
        let _ = std::fs::remove_dir_all(proxy_dir.parent().unwrap());
    }

    #[test]
    fn push_to_nonlocal_remote_errors() {
        let dir = tmp("nonlocal");
        let git = GitBackend::init_sample(&dir).unwrap();
        assert!(git.push("ssh://git@host/o/r").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
