# bosync

A proof-of-concept that makes a **Git repository behave like OneDrive / Google Drive** in
Windows Explorer, using the **Windows Cloud Filter API** (`cldapi.dll`) — the same OS
feature OneDrive, Dropbox and Google Drive for Desktop are built on.

Files in the drive appear instantly as zero-byte **placeholders** projected from a git
commit. Opening one triggers **hydration**: bosync streams the matching git blob into the
file on demand. Editing a file **commits it back** to git.

Written in (almost) pure Rust — see [`PLAN.html`](./PLAN.html) for the full design.

## How it works

```
Explorer ──read/browse──▶ cldapi.dll ──callbacks──▶ bosync (Rust) ──▶ gix ──▶ git repo
                                       FETCH_DATA            fetch_data      read blob
                                       FETCH_PLACEHOLDERS    fetch_placeh.   list tree
                                       NOTIFY_CLOSE/DELETE   closed/delete   commit
```

**Reads** are served by the Cloud Filter callbacks; **writes** are driven by a filesystem
watcher (see note below).

| Trigger | bosync does |
|---|---|
| `fetch_placeholders` callback | List the git tree for that folder, emit one placeholder per entry (lazy) |
| `fetch_data` callback | Resolve path → blob, stream bytes back in 64 KiB aligned chunks |
| `delete` callback | Allow the OS to remove the placeholder |
| watcher: file added/modified | Commit it (`Add`/`Update`) — unless it equals what's in git — then mark the file in-sync |
| watcher: file removed | Commit the removal (`Delete`) |
| `rename` / `renamed` callback | Approve the move, then commit it in git as a single `Rename old -> new` |

A write-back is a **real git commit**: it updates `HEAD`, mirrors the change into the backing
repo's **working tree**, and resets the **index** to match — so the files show up in the
backing folder and `git status` stays clean (it behaves like `git add` + `git commit`, not a
detached object-store write). After committing, the file is marked **in-sync** with Cloud
Filter so Explorer stops showing the "syncing" overlay.

### Why a watcher for write-back?

Cloud Filter's per-file `NOTIFY_FILE_CLOSE_COMPLETION` callback proved unreliable for
detecting *edits* and *new files* in testing (it fires on open/delete but not consistently
on modify). So write-back is driven by a recursive [`notify`](https://github.com/notify-rs/notify)
watcher on the drive — the same "watch the tree" approach real sync clients use. A short
debounce avoids reading half-written files, and a content-equality check means hydration
writes (which reproduce the git blob exactly) never produce spurious commits.

## Crates

| Crate | Role |
|---|---|
| `bosync-git` | Git backend over [`gix`](https://github.com/GitoxideLabs/gitoxide) (pure Rust): list dir, read/hash blob, walk tree, commit. Cross-platform. |
| `bosync-sync` | Transport-agnostic **sync interview** logic (JSON in / JSON out). Cross-platform; depends only on `bosync-git` + `serde`. |
| `bosync-cf` | Sync-root registration + session connect (wraps the `cloud-filter` crate). Windows. |
| `bosync-core` | The `SyncFilter` engine that bridges callbacks ↔ git. Windows. |
| `bosync-cli` | `bosync` binary: `mount`, `sample`, `sync`, `unmount`. |

## Requirements

- Windows 10 1709+ / Windows 11 (Cloud Filter API)
- Rust stable, MSVC toolchain (`x86_64-pc-windows-msvc`)

## Usage

```powershell
# Build
cargo build --release

# Create a sample backing repo to play with
cargo run -p bosync-cli -- sample --repo C:\tmp\bosync-repo

# Mount it as a drive (the drive folder is created if missing)
cargo run -p bosync-cli -- mount --repo C:\tmp\bosync-repo --drive C:\tmp\Mybosync

# Mount read-only: serve reads, never commit local changes back
cargo run -p bosync-cli -- mount --repo C:\tmp\bosync-repo --drive C:\tmp\Mybosync --readonly
```

Then open `C:\tmp\Mybosync` in Explorer:

- The files from the repo's `HEAD` appear as placeholders (free up ~0 bytes).
- Open `hello.txt` → it hydrates from the git blob.
- Edit and save a file → a commit lands in the backing repo (`git log` to verify) — unless mounted `--readonly`.
- Delete a file → a removal commit lands.

An empty (freshly `git init`ed, no commits) repo mounts fine and shows an empty drive.

Press **Ctrl+C** in the terminal to unmount and unregister the sync root.

## Sync interview (`bosync sync`)  — the primary interface

`bosync sync` is a deliberately lightweight, proprietary protocol (not the git wire protocol)
for two things: **checking whether a held file version is current**, and **shipping the diff**
to bring it up to date. Its main mode is `--serve` (streaming JSON), meant to run over SSH or
embedded in an SSH server; the local `--drive` mode is a convenience wrapper.

A client asks the repo, per file, *"is the version I hold current?"* — without the repo ever
reading blob contents for the check (it walks tree objects and compares ids, so it's cheap).

### Why not just a git server?

A normal git server has no single round-trip "is file X at oid Z stale, and if so give me the
delta" call — you'd `ls-remote`/`fetch` and diff locally. Here the client sends *its* oid, the
server compares it to its own and (for `patch`) returns just a unified diff. So "am I working on
a stale file?" is one cheap request, and updating is a diff, not a re-download.

```powershell
# Print the repo's file manifest (path, blob id, size)
bosync sync --repo C:\tmp\bosync-repo --manifest

# Interview a local drive folder: which files are stale / gone / new?
bosync sync --repo C:\tmp\bosync-repo --drive C:\tmp\Mybosync
#   [ok   ] README.md
#   [stale] hello.txt        <- locally edited, differs from repo
#   [new   ] docs/guide.md   <- in repo, not held locally

# Server mode: newline-delimited JSON requests in, replies out
bosync sync --repo C:\tmp\bosync-repo --serve
```

### Protocol (JSON)

Four operations. Requests and replies are one JSON object each (newline-delimited when
streaming via `--serve`).

```jsonc
// 1) manifest — full listing at a rev (default HEAD)
{"op":"manifest"}
{"op":"manifest","rev":"283b...","files":[{"path":"hello.txt","oid":"7435...","size":53}]}

// 2) status — "is the version I hold current?" (set new_files:false for a cheap single-file check)
{"op":"status","new_files":false,"files":[{"path":"hello.txt","oid":"7435..."}]}
{"op":"status","rev":"283b...","files":[{"path":"hello.txt","state":"up_to_date"}],"missing":[]}
//   states: up_to_date | out_of_date (+current_oid) | not_in_repo

// 3) patch — "bring my version X up to current Y" (oid = the client's base version)
{"op":"patch","files":[{"path":"poem.txt","oid":"d531..."}]}
{"op":"patch","rev":"757...","files":[
  {"path":"poem.txt","result":"patch","from":"d531...","to":"eb53...","diff":"--- a/poem.txt\n+++ b/poem.txt\n@@ -1,2 +1,3 @@\n..."}
]}
//   results: up_to_date | not_in_repo | patch (unified diff) |
//            replace (binary, or the client's base isn't in the repo → re-fetch whole file at `to`)

// 4) push — upload a new version (write direction, for collaboration). base_oid is the
//    version your edit was based on; the server rejects with a conflict if it moved on.
{"op":"push","path":"hello.txt","base_oid":"7435...","content":"new text\n"}
{"op":"push","path":"hello.txt","result":"committed","oid":"e9b7..."}
//   results: committed {oid} | unchanged {oid} | conflict {current_oid}
```

`rev` is optional everywhere (defaults to `HEAD`); any commit-ish works, so a client can also
compare against a tag or branch.

### Using it as a library / in an SSH server

`bosync-sync` is transport-agnostic and depends on **neither russh nor ssh-bench**. The same
logic runs three ways:

- **In-process:** `bosync_sync::handle(&repo, request) -> SyncResponse`
- **One-shot string:** `bosync_sync::handle_json(&repo, &line) -> String`
- **Streaming:** `bosync_sync::serve(&repo, reader, writer)` over any `BufRead`/`Write`

To expose it as a command in a [russh](https://github.com/Eugeny/russh) or
[ssh-bench](https://github.com/Skabunkel/ssh-bench) server, hand the SSH channel's
reader/writer to `serve` (or call `handle_json` per message) — that's the whole integration.
See [`crates/bosync-sync/examples/embed_ssh.rs`](crates/bosync-sync/examples/embed_ssh.rs):

```powershell
echo '{"op":"manifest"}' | cargo run -p bosync-sync --example embed_ssh -- C:\tmp\bosync-repo
```

Over plain SSH (no embedding) it also just works: `ssh host bosync sync --repo R --serve`.

## Collaboration (design)

The pieces are now in place for multiple people to share one repo. The model is a
**central repo reached over the sync protocol**, with each client mounting its own local
working copy:

```
   client A  ──bosync sync (SSH)──┐
                                  ▼
                          shared repo on server  ( bosync sync --serve )
                                  ▲
   client B  ──bosync sync (SSH)──┘
```

The protocol is now bidirectional, which is what makes this possible:

- **Pull**: `manifest` (what exists) + `status` (am I stale?) + `patch` (give me the delta).
- **Push**: `push` uploads a new version with **optimistic-concurrency** conflict detection
  — the client sends the `base_oid` it edited from; if the server's version has moved on,
  the push is rejected as a `conflict` (with the server's `current_oid`) instead of
  clobbering the other person's work.

A client sync loop looks like: `status` to find stale/new files → `patch`/hydrate to catch
up → `push` local edits → on `conflict`, pull again and merge. Every change is an ordinary
git commit on the server, so history, blame, and `git log` all work.

**Not yet built** (the honest gaps): no automatic merge of a conflict (the client is told
there's a conflict but must resolve it — three-way merge is the natural next step); pushes
carry whole-file UTF-8 content (patch-based and binary push are extensions); no presence
/ locking / live notification (clients poll); and auth is whatever the SSH layer provides.
Topology is a choice to make — central server (above) vs. peer-to-peer git remotes — and
that decision drives the merge story.

## Scope (this POC)

**In:** single local repo, lazy placeholder projection of `HEAD`, on-demand hydration,
local edit/delete → commit.

**Out (for now):** remote `pull`/`push`, merge-conflict resolution, rename tracking,
large-file streaming tuning, re-dehydration after commit, installer/signing. See
`PLAN.html` §10 for risks and the milestone roadmap.

## Notes & known limitations

- **First-access enumeration handshake:** the first time a folder is browsed (or a file in
  a not-yet-populated folder is opened), the call may return *"the cloud operation is
  invalid"* while bosync populates placeholders asynchronously; the next access succeeds.
  This is the standard on-demand population behavior — Explorer retries automatically, but
  single-shot API callers (e.g. one `[System.IO.File]::ReadAllText`) may see it once.
- Write-back commits onto the **currently checked-out branch** and overwrites matching
  working-tree files. Point bosync at a repo whose working tree you don't mind it driving
  (a clean repo, or a dedicated branch/clone) — it is not meant to share a working tree with
  edits you're making by hand at the same time.
- `fetch_data` reads the whole blob into memory; fine for text-sized files, not tuned for
  multi-GB blobs.
- Renames/moves within the drive are committed as a single `Rename old -> new` (handled in
  the `renamed` callback, after the OS completes the move, so an online-only file stays
  hydratable throughout). Folder renames move the whole subtree, and the renamed item (and
  its ancestors) are marked in-sync immediately so they don't get stuck "syncing".
  Hydration resolves a file by its current path, so renamed online-only files still fetch
  correctly.
- Cleanup after an unclean exit: `bosync unmount` unregisters a stale sync root.
- The sync root registers with the **Full** hydration policy. "Free up space" (explicit
  dehydration to online-only) is **not supported** under this policy — it's a known
  limitation, not yet implemented. See the note below.

### Known limitation: "Free up space" / dehydration

Making a file online-only via Explorer's "Free up space" does not currently work. It requires
the on-demand (`Partial`) hydration policy plus correct per-placeholder pin/in-sync state, and
in testing that combination regressed the normal sync overlays (files showed stuck "pending").
The provider is intentionally kept on the `Full` policy where browsing, hydration, editing,
renaming, and deletion all work correctly. Supporting dehydration cleanly is future work.

## Verified working

Against the sample repo, end-to-end on Windows 11:
- Placeholder projection of the root **and** nested folders from `HEAD` ✓
- On-demand hydration of files from git blobs ✓
- Edit a file → `Update` commit; create a (nested) file → `Add` commit; delete a file →
  `Delete` commit ✓
- Hydration produces no spurious commits ✓
