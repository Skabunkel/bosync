//! The OS-specific cloud-sync layer, abstracted so the core stays platform-agnostic.
//!
//! `bosync-core` owns the cross-platform work — the shallow-clone proxy lifecycle and the
//! git write-back. *How* a file is shown as "cloud-only", hydrated on open, and re-dehydrated
//! when idle is platform-specific (Windows Cloud Filter, Linux FUSE, macOS File Provider).
//! Each platform implements [`CloudSync`]; the core is generic over it and never names a
//! concrete OS type.

use std::path::Path;

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
