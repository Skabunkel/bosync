//! Windows integration for bosync: everything that touches the Cloud Filter API.
//!
//! - [`SyncRoot`] — register the proxy folder with Windows and connect a filter session.
//! - [`BosyncFilter`] — the `SyncFilter` provider that projects the proxy tree into
//!   placeholders and hydrates files from git on open.
//! - [`WindowsCloudSync`] — the [`bosync_core::CloudSync`] implementation the shared engine
//!   drives for the platform-specific bits of write-back (is-a-file-dehydrated, mark-in-sync,
//!   re-dehydrate).
//!
//! All of it is gated to Windows; on Linux/macOS this crate compiles to nothing, so the whole
//! workspace builds on any host. The cross-platform engine lives in `bosync-core`.

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::{mark_tree_in_sync, BosyncFilter, SyncRoot, WindowsCloudSync};
