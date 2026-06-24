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

/// How often the background loop runs `git fetch --depth=1` to keep the proxy current.
#[cfg(windows)]
const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// Run one background refresh tick: fetch the branch, and for every entry the remote changed,
/// re-dehydrate it on the drive back to a cloud-only "remote" placeholder.
#[cfg(windows)]
fn refresh_tick(
    proxy: &mut bosync_core::Proxy,
    drive: &std::path::Path,
    branch: &str,
    cloud: &bosync_windows::WindowsCloudSync,
) {
    use bosync_core::{CloudSync, ProxyState};

    match proxy.refresh_branch(branch) {
        Ok(changed) if !changed.is_empty() => {
            tracing::info!(count = changed.len(), "remote advanced; re-dehydrating changed entries");
            for rel in changed {
                let abs = drive.join(rel.replace('/', "\\"));
                cloud.mark_state(drive, &abs, ProxyState::Remote);
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("background refresh failed: {e:#}"),
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
    use std::time::{Duration, Instant};

    use notify::{RecursiveMode, Watcher};

    use bosync_core::{reconcile_path, CloudSync, Proxy, ProxyState, Reconciled};
    use bosync_windows::{mark_tree_in_sync, BosyncFilter, SyncRoot, WindowsCloudSync};

    // Shallow-clone the remote into the internal per-user proxy (the git backend). This is the
    // "create a proxy" step — a thin shallow copy, not a full clone.
    let mut proxy = Proxy::open_or_clone_in(&remote, &Proxy::proxy_dir(&remote), depth)
        .with_context(|| format!("shallow-cloning {remote}"))?;
    let git = proxy.git().clone();

    // The drive is the folder the user browses; it starts empty so every entry projects as a
    // cloud-only "remote" placeholder.
    let drive = into.unwrap_or_else(|| default_drive(&remote));
    std::fs::create_dir_all(&drive).with_context(|| format!("creating drive folder {drive:?}"))?;
    // Canonicalize so the root matches the long-form paths the OS reports in callbacks, then
    // strip the `\\?\` verbatim prefix which Cloud Filter registration rejects.
    let drive = strip_verbatim(
        std::fs::canonicalize(&drive)
            .with_context(|| format!("canonicalizing drive folder {drive:?}"))?,
    );

    let sync_root = SyncRoot::register(PROVIDER, DISPLAY_NAME, VERSION, &drive)?;
    let filter = BosyncFilter::new(git.clone(), drive.clone());
    let _connection = sync_root.connect(&drive, filter)?;

    // Clear any stale "pending sync" state from a previous session (especially folders).
    mark_tree_in_sync(&drive);

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))
        .context("setting Ctrl-C handler")?;

    let mode = if readonly { " (read-only)" } else { "" };
    println!("bosync mounted{mode}: {remote} -> {}", drive.display());
    println!("Open it in Explorer. Entries start as cloud-only; Ctrl+C to unmount.");

    // The platform seam: skips dehydrated placeholders and drives the Explorer overlays.
    let cloud = WindowsCloudSync;
    let mut last_refresh = Instant::now();

    if readonly {
        // No write-back; just keep the proxy current in the background.
        while running.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(300));
            if last_refresh.elapsed() >= REFRESH_INTERVAL {
                last_refresh = Instant::now();
                refresh_tick(&mut proxy, &drive, &branch, &cloud);
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

        // Debounce: collect changed paths, reconcile each after a short quiet period.
        let mut pending: HashSet<PathBuf> = HashSet::new();
        while running.load(Ordering::SeqCst) {
            match rx.recv_timeout(Duration::from_millis(300)) {
                Ok(event) => {
                    for path in event.paths {
                        pending.insert(path);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    for path in pending.drain() {
                        match reconcile_path(&git, &cloud, &drive, &path) {
                            // Committed to the proxy → Local. Then push it to the remote; if that
                            // lands (always, for a local target), promote to Synced so the
                            // "syncing" overlay clears. A failed push (network remote) leaves it
                            // Local and we retry next time.
                            Ok(Reconciled::Committed) => {
                                cloud.mark_state(&drive, &path, ProxyState::Local);
                                match git.push(&remote) {
                                    Ok(()) => cloud.mark_state(&drive, &path, ProxyState::Synced),
                                    Err(e) => tracing::warn!(?path, "push failed (stays local): {e:#}"),
                                }
                            }
                            Ok(Reconciled::Removed) => {
                                if let Err(e) = git.push(&remote) {
                                    tracing::warn!(?path, "push of removal failed: {e:#}");
                                }
                            }
                            Ok(Reconciled::Unchanged) => {}
                            Err(e) => tracing::warn!(?path, "reconcile failed: {e:#}"),
                        }
                    }
                    if last_refresh.elapsed() >= REFRESH_INTERVAL {
                        last_refresh = Instant::now();
                        refresh_tick(&mut proxy, &drive, &branch, &cloud);
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
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
