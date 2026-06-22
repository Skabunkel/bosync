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
    /// Mount a git repository as a virtual drive at <drive>. (Windows only for now.)
    Mount {
        /// Path to the backing git repository (the on-disk proxy).
        #[arg(long)]
        repo: PathBuf,
        /// Empty folder to expose as the drive (the sync root).
        #[arg(long)]
        drive: PathBuf,
        /// Mount read-only: serve files for reading but never commit local changes back.
        #[arg(long)]
        readonly: bool,
    },
    /// Create a sample git repository to mount.
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
                "Now run:  bosync mount --repo {} --drive <empty-folder>",
                repo.display()
            );
            Ok(())
        }
        Command::Mount {
            repo,
            drive,
            readonly,
        } => mount(repo, drive, readonly),
        Command::Unmount => unmount(),
    }
}

#[cfg(windows)]
fn mount(repo: PathBuf, drive: PathBuf, readonly: bool) -> Result<()> {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    use notify::{RecursiveMode, Watcher};

    use bosync_core::reconcile_path;
    use bosync_windows::{mark_tree_in_sync, BosyncFilter, SyncRoot, WindowsCloudSync};

    std::fs::create_dir_all(&drive).with_context(|| format!("creating drive folder {drive:?}"))?;
    // Canonicalize so the root matches the long-form paths the OS reports in callbacks, then
    // strip the `\\?\` verbatim prefix which Cloud Filter registration rejects.
    let drive = strip_verbatim(
        std::fs::canonicalize(&drive)
            .with_context(|| format!("canonicalizing drive folder {drive:?}"))?,
    );

    let git = GitBackend::open(&repo)?;

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
    println!("bosync mounted{mode}: {} -> {}", repo.display(), drive.display());
    println!("Open it in Explorer. Press Ctrl+C to unmount.");

    if readonly {
        while running.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(200));
        }
    } else {
        // The platform seam used during write-back: skips dehydrated placeholders and clears
        // Explorer overlays after each commit.
        let cloud = WindowsCloudSync;

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
                        if let Err(e) = reconcile_path(&git, &cloud, &drive, &path) {
                            tracing::warn!(?path, "reconcile failed: {e:#}");
                        }
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
fn mount(repo: PathBuf, drive: PathBuf, readonly: bool) -> Result<()> {
    // The FUSE projection is read-only today; accept the flag for parity with Windows.
    let _ = readonly;
    std::fs::create_dir_all(&drive)
        .with_context(|| format!("creating mountpoint {drive:?}"))?;
    let git = GitBackend::open(&repo)?;
    println!(
        "bosync mounting (read-only FUSE): {} -> {}",
        repo.display(),
        drive.display()
    );
    println!("Press Ctrl+C, or run:  fusermount3 -u {}", drive.display());
    bosync_linux::mount(git, &drive) // blocks until unmounted
}

#[cfg(not(any(windows, target_os = "linux")))]
fn mount(_repo: PathBuf, _drive: PathBuf, _readonly: bool) -> Result<()> {
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
