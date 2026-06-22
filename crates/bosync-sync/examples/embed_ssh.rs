//! How to expose `bosync sync` as a command inside an SSH server (russh, ssh-bench, …)
//! WITHOUT depending on those crates. The whole integration is one function over a
//! reader/writer — wire it to the SSH channel's streams in your server.
//!
//! Run it standalone to try the protocol:
//!     echo '{"op":"manifest"}' | cargo run -p bosync-sync --example embed_ssh -- <repo>

use std::io::{BufReader, Read, Write};

use bosync_git::GitBackend;

/// The integration point. In a russh `Handler::exec_request` or an ssh-bench command
/// callback, you already have the channel's input and output; hand them to this. No bosync
/// type leaks into your server beyond `GitBackend` and these two std traits.
fn bosync_sync_command(
    repo: &GitBackend,
    channel_in: impl Read,
    channel_out: impl Write,
) -> std::io::Result<()> {
    // Streaming, newline-delimited JSON. For a single request/response instead, call
    // `bosync_sync::handle_json(repo, &one_line)` and write the returned string.
    bosync_sync::serve(repo, BufReader::new(channel_in), channel_out)
}

fn main() -> anyhow::Result<()> {
    let repo_path = std::env::args()
        .nth(1)
        .expect("usage: embed_ssh <repo-path>");
    let repo = GitBackend::open(std::path::Path::new(&repo_path))?;

    // A real server passes the SSH channel's reader/writer here. We use the process'
    // stdin/stdout so the example is runnable from a shell pipe.
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    bosync_sync_command(&repo, stdin.lock(), stdout.lock())?;
    Ok(())
}
