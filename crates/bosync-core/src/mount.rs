//! The OS-specific cloud-sync layer, abstracted so the core stays platform-agnostic.
//!
//! `bosync-core` owns the cross-platform work — the shallow-clone proxy lifecycle and the
//! git write-back. *How* a file is shown as "cloud-only", hydrated on open, and re-dehydrated
//! when idle is platform-specific (Windows Cloud Filter, Linux FUSE, macOS File Provider).
//! Each platform implements [`CloudSync`]; the core is generic over it and never names a
//! concrete OS type.

use std::path::Path;

/// The sync state of one proxy entry, as shown by an overlay in the file manager.
///
/// This is the cross-platform vocabulary for the "view states when proxy is running" criterion.
/// The lifecycle, and the transitions between states, are:
///
/// ```text
///                 create / edit            push succeeds
///   Remote  ───────────────────▶  Local  ───────────────▶  Synced
///     ▲         (commit)                                       │
///     └───────────────────────────────────────────────────────┘
///            fetch --depth=1 brings a remote change
///                  (re-dehydrate to a shallow copy)
/// ```
///
/// - [`Remote`](ProxyState::Remote): the entry lives only on the remote, projected as a
///   cloud-only placeholder with no local bytes. Every entry starts here right after the proxy
///   is shallow-cloned.
/// - [`Local`](ProxyState::Local): the entry is present on disk with a change that has been
///   *committed* but not yet confirmed on the remote (push pending or unsupported). This is the
///   pending state — and the state a [`Synced`](ProxyState::Synced) entry holds while it is
///   kept on disk, until a fetch brings in a newer remote version.
/// - [`Synced`](ProxyState::Synced): the local copy has been committed **and** pushed, so it is
///   confirmed identical to the remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyState {
    /// Cloud-only placeholder; no local data. The state of every entry just after cloning.
    Remote,
    /// Present and committed locally, but not yet confirmed on the remote (push pending).
    Local,
    /// Committed and pushed: the local copy is confirmed identical to the remote.
    Synced,
}

/// The cloud-sync facility of one platform.
///
/// The methods the core actually needs to drive write-back and the idle engine in a
/// platform-neutral way. Projection and hydration themselves are owned by the platform crate
/// (they are deeply tied to each OS's callback model), so they are *not* on this trait — this
/// is only the part the shared engine calls into.
pub trait CloudSync {
    /// Whether the file at `abs` is a dehydrated (cloud-only) placeholder with no local data.
    ///
    /// Such a file cannot have been edited, so write-back skips it (reading it would just
    /// trigger a hydration whose bytes already equal the committed content). Platforms with
    /// no on-demand hydration yet report `false` — every file is treated as locally present.
    fn is_dehydrated(&self, abs: &Path) -> bool;

    /// Mark `abs` and its ancestor directories (up to and including `root`) as in-sync after
    /// a successful commit, clearing any "pending sync" overlay in the file manager.
    /// Best-effort; the default is a no-op for platforms without overlay state.
    fn mark_in_sync(&self, root: &Path, abs: &Path) {
        let _ = (root, abs);
    }

    /// Re-dehydrate `abs`, freeing its local bytes while keeping the placeholder visible.
    /// Called by the idle engine for files untouched for a while. Default no-op.
    fn dehydrate(&self, abs: &Path) {
        let _ = abs;
    }

    /// Reflect a [`ProxyState`] transition for `abs` in the file manager's overlay.
    ///
    /// This is the single seam the engine uses to drive the "view states" behaviour; it is
    /// expressed in terms of the two primitives above so platforms only need to implement
    /// those. The mapping:
    ///
    /// - [`Synced`](ProxyState::Synced) → [`mark_in_sync`](CloudSync::mark_in_sync) (green check).
    /// - [`Remote`](ProxyState::Remote) → [`dehydrate`](CloudSync::dehydrate) (cloud-only).
    /// - [`Local`](ProxyState::Local) → leave the on-disk file as-is: it is present with a
    ///   pending change, which the file manager already shows as "pending sync".
    ///
    /// Platforms with richer per-state overlays can override this to be more precise.
    fn mark_state(&self, root: &Path, abs: &Path, state: ProxyState) {
        match state {
            ProxyState::Synced => self.mark_in_sync(root, abs),
            ProxyState::Remote => self.dehydrate(abs),
            ProxyState::Local => {}
        }
    }
}

/// A [`CloudSync`] for platforms with no mount integration yet (Linux/macOS in progress) and
/// for unit tests: every file is treated as locally present and all hooks are no-ops.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullCloudSync;

impl CloudSync for NullCloudSync {
    fn is_dehydrated(&self, _abs: &Path) -> bool {
        false
    }
}
