//! The bosync sync engine — the platform-agnostic core.
//!
//! Core owns the cross-platform behaviour from the plan and names no operating system:
//!
//! - [`Proxy`] manages the on-disk proxy (a shallow clone of the remote): clone, refresh on
//!   listing, commit-and-push on save, prune and re-dehydrate while idle.
//! - [`reconcile_path`] turns a single local change under the proxy into a git commit.
//! - [`CloudSync`] is the seam to the OS: each platform crate (Windows Cloud Filter, and
//!   later Linux FUSE / macOS File Provider) implements it; core is generic over it and
//!   prefers static dispatch.
//!
//! The Windows Cloud Filter provider lives in `bosync-windows`, which implements [`CloudSync`]
//! and drives [`reconcile_path`] from its filesystem watcher.

mod mount;
mod proxy;
mod reconcile;

pub use mount::{CloudSync, NullCloudSync, ProxyState};
pub use proxy::{repo_name, Proxy, DEFAULT_DEPTH};
pub use reconcile::{reconcile_path, to_git_path, Reconciled};

#[cfg(test)]
mod testutil {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A unique temp directory removed on drop — a tiny stand-in for the `tempfile` crate so
    /// the core stays dependency-light. Test-only.
    pub struct TmpDir(PathBuf);

    impl TmpDir {
        pub fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "bosync-test-{}-{}-{n}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    use std::cell::RefCell;
    use std::collections::HashMap;

    use crate::{CloudSync, ProxyState};

    /// A [`CloudSync`] that records the last [`ProxyState`] requested per path, so tests can
    /// assert the engine drives the documented transitions without a real OS overlay.
    #[derive(Default)]
    pub struct RecordingCloudSync {
        states: RefCell<HashMap<PathBuf, ProxyState>>,
    }

    impl RecordingCloudSync {
        pub fn state_of(&self, abs: &Path) -> Option<ProxyState> {
            self.states.borrow().get(abs).copied()
        }
    }

    impl CloudSync for RecordingCloudSync {
        fn is_dehydrated(&self, abs: &Path) -> bool {
            self.state_of(abs) == Some(ProxyState::Remote)
        }

        fn mark_state(&self, _root: &Path, abs: &Path, state: ProxyState) {
            self.states.borrow_mut().insert(abs.to_path_buf(), state);
        }
    }
}
