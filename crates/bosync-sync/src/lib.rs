//! Transport-agnostic "sync interview" for bosync.
//!
//! A client holds files (the materialized contents of a bosync drive) and wants to know,
//! per file, whether its version matches the repo. It computes the git blob id of each
//! local file and asks the repo: *is this current?* The repo answers without ever reading
//! blob contents — it only walks tree objects and compares ids — so an interview is cheap.
//!
//! # Three ways to run the same logic
//!
//! 1. **In-process / CLI** — call [`handle`] directly, or [`scan_drive`] + [`handle`].
//! 2. **Over `ssh host bosync sync --serve`** — the CLI calls [`serve`] on stdin/stdout.
//! 3. **Embedded in an SSH server (russh, [ssh-bench], …)** — the server's command/exec
//!    handler calls [`serve`] with the channel's reader/writer, or [`handle_json`] per
//!    message. This crate depends on **neither** russh nor ssh-bench — it only needs a
//!    `BufRead`/`Write` (streaming) or a `&str` (one-shot), so it is trivial to graft onto
//!    any server. See `examples/embed_ssh.rs`.
//!
//! [ssh-bench]: https://github.com/Skabunkel/ssh-bench
//!
//! The wire format is JSON; for streaming it is newline-delimited JSON (one request and
//! one response per line).

use std::collections::HashSet;
use std::io::{BufRead, Write};

use serde::{Deserialize, Serialize};

use bosync_git::GitBackend;

/// A request to the sync engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SyncRequest {
    /// Return the full file listing at `rev` (default `HEAD`).
    Manifest {
        #[serde(default)]
        rev: Option<String>,
    },
    /// Compare the client's held versions against the repo.
    Status {
        #[serde(default)]
        rev: Option<String>,
        /// The files the client currently holds, with their git blob ids.
        files: Vec<ClientFile>,
        /// Also report repo files the client didn't list (i.e. new files to fetch).
        #[serde(default = "default_true")]
        new_files: bool,
    },
    /// Generate patches bringing the client's held versions up to the repo's current ones.
    /// `oid` on each file is the client's *base* version (X); the diff is X -> current (Y).
    Patch {
        #[serde(default)]
        rev: Option<String>,
        files: Vec<ClientFile>,
    },
    /// Upload a new version of a file (collaboration / write direction). `base_oid` is the
    /// version the edit was based on; the server rejects the push as a conflict if its
    /// current version has moved on since then (optimistic concurrency).
    Push {
        path: String,
        #[serde(default)]
        base_oid: Option<String>,
        /// New UTF-8 content. (Binary push is a future extension.)
        content: String,
    },
}

fn default_true() -> bool {
    true
}

/// A file the client holds: its repo-relative path and the git blob id of its content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientFile {
    pub path: String,
    pub oid: String,
}

/// A file present in the repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestFile {
    pub path: String,
    pub oid: String,
    pub size: u64,
}

/// The verdict for one client-held file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FileState {
    /// The client's version matches the repo.
    UpToDate,
    /// The repo has a different version; re-fetch `current_oid`.
    OutOfDate { current_oid: String },
    /// The path no longer exists in the repo (deleted upstream).
    NotInRepo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileStatus {
    pub path: String,
    #[serde(flatten)]
    pub state: FileState,
}

/// The patch (or fallback) for one client-held file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum PatchResult {
    /// The client's version already matches the repo.
    UpToDate,
    /// The path no longer exists in the repo.
    NotInRepo,
    /// A unified text diff from `from` (the client's version) to `to` (current).
    Patch {
        from: String,
        to: String,
        diff: String,
    },
    /// Can't produce a text diff (binary content, or the client's base isn't in the repo):
    /// the client should re-fetch the whole file at `to`.
    Replace { to: String, reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePatch {
    pub path: String,
    #[serde(flatten)]
    pub result: PatchResult,
}

/// The outcome of a [`SyncRequest::Push`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum PushResult {
    /// The new version was committed; `oid` is its blob id in the repo now.
    Committed { oid: String },
    /// The pushed content already matched the repo; nothing to do.
    Unchanged { oid: String },
    /// The repo's version moved on since `base_oid` — the client must pull and reconcile.
    /// `current_oid` is the repo's version now (absent if the path was deleted upstream).
    Conflict { current_oid: Option<String> },
}

/// The engine's reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SyncResponse {
    Manifest {
        rev: Option<String>,
        files: Vec<ManifestFile>,
    },
    Status {
        rev: Option<String>,
        /// One verdict per file the client reported.
        files: Vec<FileStatus>,
        /// Repo files the client did not report (only when `new_files` was set).
        missing: Vec<ManifestFile>,
    },
    Patch {
        rev: Option<String>,
        files: Vec<FilePatch>,
    },
    Push {
        path: String,
        #[serde(flatten)]
        result: PushResult,
    },
    Error {
        message: String,
    },
}

/// Run one request against `repo`. Pure and total: any failure becomes [`SyncResponse::Error`].
pub fn handle(repo: &GitBackend, request: SyncRequest) -> SyncResponse {
    match request {
        SyncRequest::Manifest { rev } => {
            let rev = rev.unwrap_or_else(|| "HEAD".into());
            match repo.walk(&rev) {
                Ok(list) => SyncResponse::Manifest {
                    rev: repo.rev_id(&rev),
                    files: list.into_iter().map(into_manifest).collect(),
                },
                Err(e) => SyncResponse::Error {
                    message: e.to_string(),
                },
            }
        }
        SyncRequest::Status {
            rev,
            files,
            new_files,
        } => {
            let rev = rev.unwrap_or_else(|| "HEAD".into());
            let statuses = files
                .iter()
                .map(|f| {
                    let state = match repo.path_oid(&rev, &f.path) {
                        Some(current) if current == f.oid => FileState::UpToDate,
                        Some(current) => FileState::OutOfDate { current_oid: current },
                        None => FileState::NotInRepo,
                    };
                    FileStatus {
                        path: f.path.clone(),
                        state,
                    }
                })
                .collect();

            let missing = if new_files {
                let held: HashSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
                repo.walk(&rev)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|(path, _, _)| !held.contains(path.as_str()))
                    .map(into_manifest)
                    .collect()
            } else {
                Vec::new()
            };

            SyncResponse::Status {
                rev: repo.rev_id(&rev),
                files: statuses,
                missing,
            }
        }
        SyncRequest::Patch { rev, files } => {
            let rev = rev.unwrap_or_else(|| "HEAD".into());
            let files = files
                .iter()
                .map(|f| FilePatch {
                    path: f.path.clone(),
                    result: patch_one(repo, &rev, &f.path, &f.oid),
                })
                .collect();
            SyncResponse::Patch {
                rev: repo.rev_id(&rev),
                files,
            }
        }
        SyncRequest::Push {
            path,
            base_oid,
            content,
        } => {
            let current = repo.path_oid("HEAD", &path);
            // Optimistic concurrency: reject if the repo's version isn't what the client
            // based its edit on.
            let conflict = match (&base_oid, &current) {
                (Some(base), Some(cur)) => base != cur, // changed upstream
                (Some(_), None) => true,                // deleted upstream
                (None, Some(_)) => true,                // client thought it was new, but it exists
                (None, None) => false,                  // genuinely new
            };
            if conflict {
                return SyncResponse::Push {
                    path,
                    result: PushResult::Conflict {
                        current_oid: current,
                    },
                };
            }
            // No-op if the content already matches.
            if let Ok(new_oid) = repo.hash_blob(content.as_bytes()) {
                if current.as_deref() == Some(new_oid.as_str()) {
                    return SyncResponse::Push {
                        path,
                        result: PushResult::Unchanged { oid: new_oid },
                    };
                }
            }
            match repo.commit_upsert(&path, content.as_bytes(), &format!("Push {path}")) {
                Ok(()) => {
                    let oid = repo.path_oid("HEAD", &path).unwrap_or_default();
                    SyncResponse::Push {
                        path,
                        result: PushResult::Committed { oid },
                    }
                }
                Err(e) => SyncResponse::Error {
                    message: e.to_string(),
                },
            }
        }
    }
}

/// Diff a client's base version (`base_oid`) of `path` against the repo's current version.
fn patch_one(repo: &GitBackend, rev: &str, path: &str, base_oid: &str) -> PatchResult {
    let current = match repo.path_oid(rev, path) {
        Some(y) => y,
        None => return PatchResult::NotInRepo,
    };
    if current == base_oid {
        return PatchResult::UpToDate;
    }
    let current_bytes = match repo.read_oid(&current) {
        Some(b) => b,
        None => {
            return PatchResult::Replace {
                to: current,
                reason: "current_unavailable".into(),
            }
        }
    };
    let base_bytes = match repo.read_oid(base_oid) {
        Some(b) => b,
        None => {
            // The client's base version isn't in the repo (e.g. a local-only edit): we can't
            // diff from it, so tell the client to take the whole current file.
            return PatchResult::Replace {
                to: current,
                reason: "base_unavailable".into(),
            };
        }
    };

    match (
        std::str::from_utf8(&base_bytes),
        std::str::from_utf8(&current_bytes),
    ) {
        (Ok(old), Ok(new)) => {
            let old_name = format!("a/{path}");
            let new_name = format!("b/{path}");
            let text_diff = similar::TextDiff::from_lines(old, new);
            let mut unified = text_diff.unified_diff();
            unified.header(&old_name, &new_name);
            PatchResult::Patch {
                from: base_oid.to_string(),
                to: current,
                diff: unified.to_string(),
            }
        }
        _ => PatchResult::Replace {
            to: current,
            reason: "binary".into(),
        },
    }
}

fn into_manifest((path, oid, size): (String, String, u64)) -> ManifestFile {
    ManifestFile { path, oid, size }
}

/// One-shot: parse a JSON request, handle it, and serialize the JSON reply. Never errors —
/// malformed input and serialization failures both come back as an `error` response.
/// This is the lightest possible entry point for an SSH command handler.
pub fn handle_json(repo: &GitBackend, input: &str) -> String {
    let response = match serde_json::from_str::<SyncRequest>(input) {
        Ok(request) => handle(repo, request),
        Err(e) => SyncResponse::Error {
            message: format!("invalid request: {e}"),
        },
    };
    serde_json::to_string(&response).unwrap_or_else(|e| {
        format!(r#"{{"op":"error","message":"serialize failed: {e}"}}"#)
    })
}

/// Streaming: read newline-delimited JSON requests from `input` and write one
/// newline-delimited JSON response per request to `output`.
///
/// This is the function to wire into an SSH server: pass the channel's reader and writer.
/// It is generic over `BufRead`/`Write`, so it has no knowledge of the transport.
pub fn serve(
    repo: &GitBackend,
    input: impl BufRead,
    mut output: impl Write,
) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = handle_json(repo, &line);
        output.write_all(response.as_bytes())?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    Ok(())
}

/// Build a [`SyncRequest::Status`] payload by scanning a local bosync drive folder.
///
/// Materialized files are hashed; dehydrated placeholders (no local data) are reported with
/// the repo's current id, since by construction they equal the upstream version and were
/// never edited. Windows-only (it inspects cloud-file attributes).
#[cfg(windows)]
pub fn scan_drive(repo: &GitBackend, root: &std::path::Path) -> anyhow::Result<Vec<ClientFile>> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
    const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            let path = entry.path();
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let git_path = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");

            let attrs = meta.file_attributes();
            if attrs & (FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS) != 0 {
                // Dehydrated placeholder: in sync by construction; report the repo's id.
                if let Some(oid) = repo.path_oid("HEAD", &git_path) {
                    out.push(ClientFile { path: git_path, oid });
                }
                continue;
            }
            let data = std::fs::read(&path)?;
            let oid = repo.hash_blob(&data)?;
            out.push(ClientFile { path: git_path, oid });
        }
    }
    Ok(out)
}
