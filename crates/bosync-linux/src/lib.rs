//! Linux integration for bosync: a FUSE filesystem that projects the proxy's git tree as a
//! virtual drive, plus the [`bosync_core::CloudSync`] implementation the shared engine drives.
//!
//! Gated to Linux; on other targets this crate compiles to nothing, so the whole workspace
//! builds anywhere. The cross-platform engine lives in `bosync-core`.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::{mount, BosyncFs, LinuxCloudSync};
