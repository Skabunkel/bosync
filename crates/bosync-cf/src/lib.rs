//! Cloud Filter sync-root lifecycle: register the folder with Windows so it shows up
//! in Explorer as a sync root, and connect a [`SyncFilter`] session to it.
//!
//! This is a thin wrapper over the `cloud-filter` crate's `root` module so the rest of
//! bosync doesn't deal with `SyncRootId` bookkeeping directly.

use std::path::Path;

use anyhow::{anyhow, Result};
use cloud_filter::filter::SyncFilter;
use cloud_filter::root::{
    Connection, HydrationType, PopulationType, SecurityId, Session, SyncRootId, SyncRootIdBuilder,
    SyncRootInfo,
};

/// A registered sync root. Registration persists in the OS until [`SyncRoot::unregister`].
pub struct SyncRoot {
    id: SyncRootId,
}

impl SyncRoot {
    /// Register `path` as a sync root for the given provider, if not already registered.
    pub fn register(
        provider: &str,
        display_name: &str,
        version: &str,
        path: &Path,
    ) -> Result<Self> {
        let id = SyncRootIdBuilder::new(provider)
            .user_security_id(
                SecurityId::current_user().map_err(|e| anyhow!("current user SID: {e:?}"))?,
            )
            .build();

        let registered = id
            .is_registered()
            .map_err(|e| anyhow!("is_registered: {e:?}"))?;
        if !registered {
            let info = SyncRootInfo::default()
                .with_display_name(display_name)
                .with_hydration_type(HydrationType::Full)
                .with_population_type(PopulationType::Full)
                .with_icon("%SystemRoot%\\system32\\imageres.dll,-1043")
                .with_version(version)
                .with_recycle_bin_uri("http://bosync.local/recyclebin")
                .map_err(|e| anyhow!("recycle bin uri: {e:?}"))?
                .with_path(path)
                .map_err(|e| anyhow!("sync root path {path:?}: {e:?}"))?;
            id.register(info).map_err(|e| anyhow!("register: {e:?}"))?;
            tracing::info!("registered sync root at {path:?}");
        } else {
            tracing::info!("sync root already registered");
        }

        Ok(Self { id })
    }

    /// Build a handle to the (possibly already-registered) sync root without registering.
    pub fn open(provider: &str) -> Result<Self> {
        let id = SyncRootIdBuilder::new(provider)
            .user_security_id(
                SecurityId::current_user().map_err(|e| anyhow!("current user SID: {e:?}"))?,
            )
            .build();
        Ok(Self { id })
    }

    /// Connect a filter session. The returned [`Connection`] stays live until dropped.
    pub fn connect<F: SyncFilter + 'static>(
        &self,
        path: &Path,
        filter: F,
    ) -> Result<Connection<F>> {
        Session::new()
            .connect(path, filter)
            .map_err(|e| anyhow!("connect session: {e:?}"))
    }

    /// Remove the OS registration for this sync root.
    pub fn unregister(&self) -> Result<()> {
        self.id
            .unregister()
            .map_err(|e| anyhow!("unregister: {e:?}"))?;
        tracing::info!("unregistered sync root");
        Ok(())
    }
}
