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

use std::path::Path;

use anyhow::{Context, Result};
use gix::object::tree::EntryKind;

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
    fn collect_blobs_under(
        &self,
        rev: &str,
        path: &str,
    ) -> Result<Option<Vec<(String, Vec<u8>)>>> {
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
