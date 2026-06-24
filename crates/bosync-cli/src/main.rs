//! bosync CLI: mount a git repository as a virtual drive, or scaffold a sample repo.
//!
//! The cross-platform commands (`sample`) work everywhere. `mount` / `unmount` are currently
//! Windows-only (Cloud Filter); the platform-agnostic proxy engine lives in `bosync-core` and
//! the Linux/macOS mounts are the next platforms to grow into it.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use bosync_git::GitBackend;

// Sync-root identity — only referenced by the Windows mount/unmount commands.
#[cfg(windows)]
const PROVIDER: &str = "Bosync";
#[cfg(windows)]
const DISPLAY_NAME: &str = "Bosync";
#[cfg(windows)]
const VERSION: &str = "0.1.0";

#[derive(Parser)]
#[command(name = "bosync", about = "A git-backed virtual drive")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Mount a remote as a live virtual drive. (Windows; Linux mounts read-only.)
    ///
    /// bosync shallow-clones the remote into an internal per-user proxy and projects it as a
    /// virtual drive: entries appear as cloud-only "remote" placeholders, hydrate on open, and
    /// commit back on save. In the background it keeps fetching `--depth=1` and re-dehydrates
    /// anything the remote changed — so the folder stays a thin, always-current mirror.
    Mount {
        /// Remote URL to mount: `ssh://`, `git@host:owner/repo`, or `http(s)://`.
        #[arg(long)]
        remote: String,
        /// Folder to expose as the live drive (created if missing). Defaults to
        /// `<cwd>/<repo-name>`.
        #[arg(long, alias = "drive")]
        into: Option<PathBuf>,
        /// Branch to track for background refresh.
        #[arg(long, default_value = "master")]
        branch: String,
        /// Shallow-clone depth — the smaller the better. Defaults to 1 (just the current tree).
        #[arg(long, default_value_t = bosync_core::DEFAULT_DEPTH)]
        depth: u32,
        /// Mount read-only: serve files for reading but never commit local changes back.
        #[arg(long)]
        readonly: bool,
    },
    /// Create a sample git repository to use as a fake remote.
    Sample {
        /// Where to create the sample repo.
        #[arg(long)]
        repo: PathBuf,
    },
    /// Unregister the bosync sync root (cleanup after an unclean exit). (Windows only.)
    Unmount,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,bosync=debug".into()),
        )
        .init();

    // Commit identity for write-back (gix reads these for author/committer).
    set_default_identity();

    match Cli::parse().command {
        Command::Sample { repo } => {
            GitBackend::init_sample(&repo)
                .with_context(|| format!("creating sample repo at {repo:?}"))?;
            println!("Created sample repo at {}", repo.display());
            println!(
                "Now run:  bosync mount --remote {}",
                repo.display()
            );
            Ok(())
        }
        Command::Mount {
            remote,
            into,
            branch,
            depth,
            readonly,
        } => mount(remote, into, branch, depth, readonly),
        Command::Unmount => unmount(),
    }
}

/// The default drive folder for a remote when `--into` isn't given: `<cwd>/<repo-name>`.
fn default_drive(remote: &str) -> PathBuf {
    let name = bosync_core::repo_name(remote);
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(name)
}

/// How long writes must settle before the watcher reconciles a batch of changes into commits.
#[cfg(windows)]
const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);

/// How often the mount loop polls the scheduler and drains filesystem events.
#[cfg(windows)]
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Whether `path` is inside the proxy's `.git` store. Changes there (our own commits) must be
/// ignored by the write-back watcher, or every commit would trigger another commit forever.
#[cfg(windows)]
fn in_git_dir(path: &std::path::Path) -> bool {
    path.components()
        .any(|c| c.as_os_str() == ".git")
}

/// Seed the jitter PRNG from wall-clock + pid so concurrent mounts don't fetch in lockstep.
#[cfg(windows)]
fn jitter_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ ((std::process::id() as u64) << 32)
}

/// Run one background refresh: fetch the branch and, for every entry the remote changed, pull the
/// new content into the proxy folder and mark it synced. Returns whether anything actually changed,
/// so the scheduler can back off when the remote is quiet.
#[cfg(windows)]
fn refresh_tick(
    proxy: &mut bosync_core::Proxy,
    drive: &std::path::Path,
    branch: &str,
    cloud: &bosync_windows::WindowsCloudSync,
) -> bool {
    use bosync_core::{CloudSync, ProxyState};

    match proxy.refresh_branch(branch) {
        Ok(changed) if !changed.is_empty() => {
            tracing::info!(count = changed.len(), "remote advanced; pulled changed entries");
            for rel in &changed {
                let abs = drive.join(rel.replace('/', "\\"));
                cloud.mark_state(drive, &abs, ProxyState::Synced);
            }
            true
        }
        Ok(_) => false,
        Err(e) => {
            tracing::warn!("background refresh failed: {e:#}");
            false
        }
    }
}

#[cfg(windows)]
fn mount(
    remote: String,
    into: Option<PathBuf>,
    branch: String,
    depth: u32,
    readonly: bool,
) -> Result<()> {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Instant;

    use notify::{RecursiveMode, Watcher};

    use bosync_core::{
        reconcile_path, CloudSync, Proxy, ProxyState, Reconciled, SyncPolicy, SyncScheduler, Tick,
    };
    use bosync_windows::{hide_dir, mark_tree_in_sync, BosyncFilter, SyncRoot, WindowsCloudSync};

    // Normalize the remote (handles `file://C:\…` on Windows) so clone/fetch/push agree.
    let remote = bosync_git::normalize_remote(&remote);

    // The mounted folder *is* the local git working copy (the proxy): a shallow clone with its
    // own `.git`. Local changes are written here and synced from here to the remote. Re-mounting
    // an existing folder reuses it.
    let drive = into.unwrap_or_else(|| default_drive(&remote));
    let mut proxy = Proxy::open_or_clone_in(&remote, &drive, depth)
        .with_context(|| format!("preparing proxy at {drive:?} from {remote}"))?;
    let git = proxy.git().clone();

    // Canonicalize so the root matches the long-form paths the OS reports in callbacks, then
    // strip the `\\?\` verbatim prefix which Cloud Filter registration rejects.
    let drive = strip_verbatim(
        std::fs::canonicalize(&drive)
            .with_context(|| format!("canonicalizing drive folder {drive:?}"))?,
    );
    // Hide the local git store so it doesn't show up in the mounted folder.
    hide_dir(&drive.join(".git"));

    let sync_root = SyncRoot::register(PROVIDER, DISPLAY_NAME, VERSION, &drive)?;
    // The filter only projects/hydrates and commits renames into the proxy; it never pushes.
    // All remote traffic is paced centrally by the scheduler in the loop below (be nice to the
    // remote), so a rename just commits locally and the loop notices HEAD moved.
    let filter = BosyncFilter::new(git.clone(), drive.clone());
    let _connection = sync_root.connect(&drive, filter)?;

    // Clear any stale "pending sync" state from a previous session (skips `.git`).
    mark_tree_in_sync(&drive);

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))
        .context("setting Ctrl-C handler")?;

    let mode = if readonly { " (read-only)" } else { "" };
    println!("bosync mounted{mode}: {remote} -> {}", drive.display());
    println!("Open it in Explorer (the .git store is hidden). Press Ctrl+C to unmount.");

    // The platform seam: skips dehydrated placeholders and drives the Explorer overlays.
    let cloud = WindowsCloudSync;

    // Paces all remote traffic: batches pushes and backs off / jitters fetches so we don't get
    // rate-limited by a hosted remote (see `bosync_core::SyncScheduler`).
    let mut scheduler = SyncScheduler::new(SyncPolicy::default(), Instant::now(), jitter_seed());

    if readonly {
        // No write-back; just keep the proxy current in the background, paced by the scheduler.
        while running.load(Ordering::SeqCst) {
            std::thread::sleep(POLL_INTERVAL);
            let now = Instant::now();
            if let Tick::Fetch = scheduler.poll(now) {
                let changed = refresh_tick(&mut proxy, &drive, &branch, &cloud);
                scheduler.note_fetched(now, changed);
            }
        }
    } else {
        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                let _ = tx.send(event);
            }
        })
        .context("creating filesystem watcher")?;
        watcher
            .watch(&drive, RecursiveMode::Recursive)
            .with_context(|| format!("watching {drive:?}"))?;

        // Debounce buffer for raw filesystem events.
        let mut pending: HashSet<PathBuf> = HashSet::new();
        let mut last_event: Option<Instant> = None;
        // Track the proxy's HEAD so we notice *any* new commit (a watcher reconcile here, or a
        // rename committed by the filter) and pace its push centrally. Start in sync with what we
        // cloned, so a freshly-mounted proxy owes the remote nothing.
        let mut last_head = git.rev_id("HEAD");
        let mut pushed_head = last_head.clone();

        while running.load(Ordering::SeqCst) {
            match rx.recv_timeout(POLL_INTERVAL) {
                Ok(event) => {
                    for path in event.paths {
                        // Ignore writes inside `.git` — those are our own commits; reconciling
                        // them would loop forever.
                        if !in_git_dir(&path) {
                            pending.insert(path);
                        }
                    }
                    last_event = Some(Instant::now());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }

            let now = Instant::now();

            // Once writes settle, reconcile each buffered path into a commit and show it as
            // "syncing" (Local). The batched push below promotes the whole tree to Synced.
            if !pending.is_empty() && last_event.is_none_or(|t| now.duration_since(t) >= DEBOUNCE) {
                for path in pending.drain() {
                    match reconcile_path(&git, &cloud, &drive, &path) {
                        Ok(Reconciled::Committed) => {
                            cloud.mark_state(&drive, &path, ProxyState::Local)
                        }
                        Ok(Reconciled::Removed | Reconciled::Unchanged) => {}
                        Err(e) => tracing::warn!(?path, "reconcile failed: {e:#}"),
                    }
                }
                last_event = None;
            }

            // A new commit (from the reconcile above or a rename in the filter) arms/extends the
            // push batch — exactly once per commit, regardless of where it came from.
            let head = git.rev_id("HEAD");
            if head != last_head {
                scheduler.note_change(now);
                last_head = head;
            }

            match scheduler.poll(now) {
                Tick::Push => {
                    if last_head != pushed_head {
                        match git.push(&remote) {
                            Ok(()) => {
                                // Everything committed is now on the remote → the whole tree is
                                // Synced (clears the "syncing" overlays in one shot).
                                mark_tree_in_sync(&drive);
                                pushed_head = last_head.clone();
                                scheduler.note_pushed(now);
                                tracing::info!("pushed batched changes to remote");
                            }
                            Err(e) => {
                                tracing::warn!("batch push failed (will retry): {e:#}");
                                scheduler.note_push_failed(now);
                            }
                        }
                    } else {
                        // Nothing genuinely unpushed (e.g. a no-op edit) — close out the batch.
                        scheduler.note_pushed(now);
                    }
                }
                Tick::Fetch => {
                    let changed = refresh_tick(&mut proxy, &drive, &branch, &cloud);
                    scheduler.note_fetched(now, changed);
                }
                Tick::Idle(_) => {}
            }
        }
    }

    drop(_connection);
    sync_root.unregister()?;
    println!("Unmounted.");
    Ok(())
}

#[cfg(target_os = "linux")]
fn mount(
    remote: String,
    into: Option<PathBuf>,
    branch: String,
    depth: u32,
    readonly: bool,
) -> Result<()> {
    use bosync_core::Proxy;

    // The FUSE projection is read-only today; accept the flag for parity with Windows. Background
    // refresh isn't wired through the blocking FUSE mount yet (Windows is the full path).
    let _ = (readonly, &branch);
    let proxy = Proxy::open_or_clone_in(&remote, &Proxy::proxy_dir(&remote), depth)
        .with_context(|| format!("shallow-cloning {remote}"))?;
    let git = proxy.git().clone();

    let drive = into.unwrap_or_else(|| default_drive(&remote));
    std::fs::create_dir_all(&drive).with_context(|| format!("creating mountpoint {drive:?}"))?;
    println!("bosync mounting (read-only FUSE): {remote} -> {}", drive.display());
    println!("Press Ctrl+C, or run:  fusermount3 -u {}", drive.display());
    bosync_linux::mount(git, &drive) // blocks until unmounted
}

#[cfg(not(any(windows, target_os = "linux")))]
fn mount(
    _remote: String,
    _into: Option<PathBuf>,
    _branch: String,
    _depth: u32,
    _readonly: bool,
) -> Result<()> {
    anyhow::bail!("`mount` is not wired up for this OS yet (macOS File Provider is pending)")
}

#[cfg(windows)]
fn unmount() -> Result<()> {
    bosync_windows::SyncRoot::open(PROVIDER)?.unregister()?;
    println!("Unregistered bosync sync root.");
    Ok(())
}

#[cfg(target_os = "linux")]
fn unmount() -> Result<()> {
    // FUSE has no persistent registration to clean up; unmount the mountpoint directly.
    anyhow::bail!("on Linux, unmount the FUSE drive with `fusermount3 -u <mountpoint>`")
}

#[cfg(not(any(windows, target_os = "linux")))]
fn unmount() -> Result<()> {
    anyhow::bail!("`unmount` is not wired up for this OS yet")
}

/// Remove the `\\?\` extended-length prefix that `canonicalize` adds on Windows.
#[cfg(windows)]
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => p,
    }
}

fn set_default_identity() {
    for (key, val) in [
        ("GIT_AUTHOR_NAME", "bosync"),
        ("GIT_AUTHOR_EMAIL", "bosync@localhost"),
        ("GIT_COMMITTER_NAME", "bosync"),
        ("GIT_COMMITTER_EMAIL", "bosync@localhost"),
    ] {
        if std::env::var_os(key).is_none() {
            std::env::set_var(key, val);
        }
    }
}
