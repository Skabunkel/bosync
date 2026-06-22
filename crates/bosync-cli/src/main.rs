//! bosync CLI: mount a git repository as a Cloud Files virtual drive, or scaffold a
//! sample repo to try it against.

use std::collections::HashSet;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use notify::{RecursiveMode, Watcher};

use bosync_cf::SyncRoot;
use bosync_core::{reconcile_path, BosyncFilter};
use bosync_git::GitBackend;
use bosync_sync::{FileState, SyncRequest, SyncResponse};

const PROVIDER: &str = "Bosync";
const DISPLAY_NAME: &str = "Bosync";
const VERSION: &str = "0.1.0";

#[derive(Parser)]
#[command(name = "bosync", about = "A git-backed virtual drive using the Windows Cloud Filter API")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Mount a git repository as a virtual drive at <drive>.
    Mount {
        /// Path to the backing git repository.
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
    /// Interview a repo: report whether held file versions are in sync. Works locally,
    /// over `ssh host bosync sync --serve`, or embedded in an SSH server (see bosync-sync).
    Sync {
        /// Path to the git repository to interview.
        #[arg(long)]
        repo: PathBuf,
        /// Compare the files in this local drive folder against the repo.
        #[arg(long)]
        drive: Option<PathBuf>,
        /// Revision to compare against (default HEAD).
        #[arg(long)]
        rev: Option<String>,
        /// Just print the repo's file manifest.
        #[arg(long)]
        manifest: bool,
        /// Server mode: read newline-delimited JSON requests on stdin, write replies on stdout.
        #[arg(long)]
        serve: bool,
        /// Emit JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Unregister the bosync sync root (cleanup after an unclean exit).
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
            println!("Now run:  bosync mount --repo {} --drive <empty-folder>", repo.display());
            Ok(())
        }
        Command::Mount {
            repo,
            drive,
            readonly,
        } => mount(repo, drive, readonly),
        Command::Sync {
            repo,
            drive,
            rev,
            manifest,
            serve,
            json,
        } => sync(repo, drive, rev, manifest, serve, json),
        Command::Unmount => {
            SyncRoot::open(PROVIDER)?.unregister()?;
            println!("Unregistered bosync sync root.");
            Ok(())
        }
    }
}

fn mount(repo: PathBuf, drive: PathBuf, readonly: bool) -> Result<()> {
    std::fs::create_dir_all(&drive)
        .with_context(|| format!("creating drive folder {drive:?}"))?;
    // Canonicalize so the root matches the long-form paths the OS reports in callbacks
    // (e.g. `C:\Users\Full Name\...`, not the 8.3 short name `NIKLA~1`). Then strip the
    // `\\?\` verbatim prefix, which Cloud Filter registration rejects.
    let drive = strip_verbatim(
        std::fs::canonicalize(&drive)
            .with_context(|| format!("canonicalizing drive folder {drive:?}"))?,
    );

    let git = GitBackend::open(&repo)?;

    let sync_root = SyncRoot::register(PROVIDER, DISPLAY_NAME, VERSION, &drive)?;
    // The filter handles reads (hydration + placeholder projection); the watcher below
    // handles writes (commit-back). Both share the git backend (cheap to clone).
    let filter = BosyncFilter::new(git.clone(), drive.clone());
    let _connection = sync_root.connect(&drive, filter)?;

    // Clear any stale "pending sync" state from a previous session (especially folders).
    // Done before the watcher starts so its conversions don't look like user edits.
    bosync_core::mark_tree_in_sync(&drive);

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || r.store(false, Ordering::SeqCst))
        .context("setting Ctrl-C handler")?;

    let mode = if readonly { " (read-only)" } else { "" };
    println!(
        "bosync mounted{mode}: {} -> {}",
        repo.display(),
        drive.display()
    );
    println!("Open it in Explorer. Press Ctrl+C to unmount.");

    if readonly {
        // No write-back: just keep the connection alive until Ctrl+C.
        while running.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(200));
        }
    } else {
        // Watch the drive for local changes and commit them back to git.
        let (tx, rx) = mpsc::channel();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(event) = res {
                    let _ = tx.send(event);
                }
            })
            .context("creating filesystem watcher")?;
        watcher
            .watch(&drive, RecursiveMode::Recursive)
            .with_context(|| format!("watching {drive:?}"))?;

        // Debounce: collect changed paths, and after a short quiet period reconcile each
        // one. The quiet window avoids reading half-written (or mid-hydration) files.
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
                        if let Err(e) = reconcile_path(&git, &drive, &path) {
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

fn sync(
    repo: PathBuf,
    drive: Option<PathBuf>,
    rev: Option<String>,
    manifest: bool,
    serve: bool,
    json: bool,
) -> Result<()> {
    let git = GitBackend::open(&repo)?;

    // Server mode: stream newline-delimited JSON over stdin/stdout. This is what runs
    // under `ssh host bosync sync --serve` and mirrors how an SSH server embeds the lib.
    if serve {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        bosync_sync::serve(&git, stdin.lock(), stdout.lock())?;
        return Ok(());
    }

    if manifest {
        let response = bosync_sync::handle(&git, SyncRequest::Manifest { rev });
        print_response(&response, json);
        return Ok(());
    }

    if let Some(drive) = drive {
        let drive = strip_verbatim(
            std::fs::canonicalize(&drive)
                .with_context(|| format!("canonicalizing drive folder {drive:?}"))?,
        );
        let files = bosync_sync::scan_drive(&git, &drive)?;
        let response = bosync_sync::handle(
            &git,
            SyncRequest::Status {
                rev,
                files,
                new_files: true,
            },
        );
        print_response(&response, json);
        return Ok(());
    }

    // One-shot: read a single JSON request from stdin (e.g. `bosync sync --repo R < req.json`).
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    println!("{}", bosync_sync::handle_json(&git, input.trim()));
    Ok(())
}

fn print_response(response: &SyncResponse, json: bool) {
    if json {
        match serde_json::to_string_pretty(response) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("error serializing: {e}"),
        }
        return;
    }
    match response {
        SyncResponse::Manifest { rev, files } => {
            println!("manifest @ {}", rev.as_deref().unwrap_or("(unborn)"));
            for f in files {
                println!("  {}  {:>8}  {}", &f.oid[..f.oid.len().min(8)], f.size, f.path);
            }
        }
        SyncResponse::Status {
            rev,
            files,
            missing,
        } => {
            println!("status @ {}", rev.as_deref().unwrap_or("(unborn)"));
            for s in files {
                let label = match s.state {
                    FileState::UpToDate => "ok  ",
                    FileState::OutOfDate { .. } => "stale",
                    FileState::NotInRepo => "gone ",
                };
                println!("  [{label}] {}", s.path);
            }
            for m in missing {
                println!("  [new  ] {}", m.path);
            }
        }
        SyncResponse::Patch { rev, files } => {
            println!("patches @ {}", rev.as_deref().unwrap_or("(unborn)"));
            for f in files {
                match &f.result {
                    bosync_sync::PatchResult::UpToDate => println!("  [ok   ] {}", f.path),
                    bosync_sync::PatchResult::NotInRepo => println!("  [gone ] {}", f.path),
                    bosync_sync::PatchResult::Replace { reason, .. } => {
                        println!("  [whole] {} ({reason})", f.path)
                    }
                    bosync_sync::PatchResult::Patch { diff, .. } => {
                        println!("  [patch] {}", f.path);
                        print!("{diff}");
                    }
                }
            }
        }
        SyncResponse::Push { path, result } => {
            println!("push {path}: {result:?}");
        }
        SyncResponse::Error { message } => eprintln!("error: {message}"),
    }
}

/// Remove the `\\?\` extended-length prefix that `canonicalize` adds on Windows.
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
