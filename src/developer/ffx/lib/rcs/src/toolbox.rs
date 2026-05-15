// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::time::{Duration, Instant};

#[derive(thiserror::Error, Debug)]
pub enum ToolboxError {
    #[error(
        "Attempted to find protocol marker {protocol_name} at \
'/toolbox', but it wasn't available. \n\n\
Make sure the target is connected and otherwise functioning, \
and that it is configured to provide capabilities over the \
network to host tools.\n\n\
If the protocol is provided by a component that is not in the \
base image, you may need to have a package server running and \
available to your target. Source: {source}
"
    )]
    ProtocolNotFoundAtToolbox {
        protocol_name: String,
        #[source]
        source: crate::RcsError,
    },

    #[error(
        "Attempted to find protocol marker {protocol_name} at \
'/toolbox' or '{backup_moniker}', but it wasn't available \
at either of those monikers.\n\n\
Make sure the target is connected and otherwise functioning, \
and that it is configured to provide capabilities over the \
network to host tools.\n\n\
If the protocol is provided by a component that is not in the \
base image, you may need to have a package server running and \
available to your target. Source: {source}
"
    )]
    ProtocolNotFoundAtBackup {
        protocol_name: String,
        backup_moniker: String,
        #[source]
        source: crate::RcsError,
    },

    #[error("FIDL error: {0}")]
    Fidl(#[from] fidl::Error),

    #[error("RCS error: {0}")]
    Rcs(#[from] crate::RcsError),

    #[cfg(not(feature = "fdomain"))]
    #[error("Moniker error: {0}")]
    Moniker(#[from] moniker::MonikerError),

    #[cfg(not(feature = "fdomain"))]
    #[error("Open directory error: {0:?}")]
    OpenDirectory(sys2::OpenError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[cfg(feature = "fdomain")]
    #[error("FDomain client error: {0}")]
    FDomain(#[from] fdomain_client::Error),
}

pub type Result<T> = std::result::Result<T, ToolboxError>;

#[cfg(feature = "fdomain")]
use {
    fdomain_client::fidl::{DiscoverableProtocolMarker, Proxy},
    fdomain_fuchsia_developer_remotecontrol::RemoteControlProxy,
    fdomain_fuchsia_io as fio,
    fdomain_fuchsia_sys2::OpenDirType,
};

#[cfg(not(feature = "fdomain"))]
use {
    fidl::endpoints::{DiscoverableProtocolMarker, ProxyHasDomain},
    fidl_fuchsia_developer_remotecontrol::RemoteControlProxy,
    fidl_fuchsia_io as fio, fidl_fuchsia_sys2 as sys2,
    fidl_fuchsia_sys2::OpenDirType,
};

pub const MONIKER: &str = "toolbox";
const LEGACY_MONIKER: &str = "core/toolbox";

#[cfg(not(feature = "fdomain"))]
async fn connect_realm_query(
    rcs: &RemoteControlProxy,
    moniker: &str,
) -> Result<sys2::RealmQueryProxy> {
    // Try to connect via fuchsia.developer.remotecontrol/RemoteControl.ConnectCapability.
    let (query, server) = fidl::endpoints::create_proxy::<sys2::RealmQueryMarker>();
    let res = rcs
        .connect_capability(
            moniker,
            sys2::OpenDirType::NamespaceDir,
            &format!("svc/{}.root", sys2::RealmQueryMarker::PROTOCOL_NAME),
            server.into_channel(),
        )
        .await;

    let res = res.map_err(crate::RcsError::Fidl)?;
    res.map_err(|e| crate::RcsError::ConnectionFailed {
        moniker: moniker.to_string(),
        capability: sys2::RealmQueryMarker::PROTOCOL_NAME.to_string(),
        error: e,
    })?;
    return Ok(query);
}

#[cfg(not(feature = "fdomain"))]
async fn open_instance_directory(
    moniker: &moniker::Moniker,
    dir_type: OpenDirType,
    realm: &sys2::RealmQueryProxy,
) -> Result<fio::DirectoryProxy> {
    let moniker_str = moniker.to_string();
    let (dir_client, dir_server) = realm.domain().create_proxy::<fio::DirectoryMarker>();
    let res = realm.open_directory(&moniker_str, dir_type.clone().into(), dir_server).await;

    let res = res.map_err(crate::RcsError::Fidl)?;
    res.map_err(ToolboxError::OpenDirectory)?;
    Ok(dir_client)
}

/// Open the service directory of the toolbox.
#[cfg(not(feature = "fdomain"))]
pub async fn open_toolbox(rcs: &RemoteControlProxy) -> Result<fio::DirectoryProxy> {
    let (query, moniker) = {
        if let Ok(query) = connect_realm_query(rcs, MONIKER).await {
            (query, MONIKER)
        } else {
            let query = connect_realm_query(rcs, LEGACY_MONIKER).await?;
            (query, LEGACY_MONIKER)
        }
    };
    let moniker = moniker::Moniker::try_from(moniker)?;
    let namespace_dir =
        open_instance_directory(&moniker, sys2::OpenDirType::NamespaceDir.into(), &query).await?;
    let (ret, server) = fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
    namespace_dir
        .open(
            "svc",
            fio::Flags::PROTOCOL_DIRECTORY | fio::PERM_READABLE,
            &fio::Options::default(),
            server.into(),
        )
        .map_err(crate::RcsError::Fidl)?;

    Ok(ret)
}

/// Open the service directory of the toolbox.
#[cfg(feature = "fdomain")]
pub async fn open_toolbox(rcs: &RemoteControlProxy) -> Result<fio::DirectoryProxy> {
    let channel = rcs.domain().namespace().await?;
    Ok(fio::DirectoryProxy::from_channel(channel))
}

/// Connects to a protocol available in the namespace of the `toolbox` component.
/// If we fail to connect to the protocol in the namespace of the `toolbox` component, then we'll
/// attempt to connect to the protocol in the exposed directory of the component located at the
/// given `backup_moniker`.
pub async fn connect_with_timeout<P>(
    rcs_proxy: &RemoteControlProxy,
    backup_moniker: Option<impl AsRef<str>>,
    dur: Duration,
) -> Result<P::Proxy>
where
    P: DiscoverableProtocolMarker,
{
    let protocol_name = P::PROTOCOL_NAME;
    // time this so that we can use an appropriately shorter timeout for the attempt
    // to connect by the backup (if there is one)
    let start_time = Instant::now();
    let toolbox_res = crate::open_with_timeout_at::<P>(
        dur,
        MONIKER,
        OpenDirType::NamespaceDir,
        &format!("svc/{protocol_name}"),
        rcs_proxy,
    )
    .await;

    // Fallback to legacy toolbox moniker if toolbox is not available.
    let toolbox_res = match toolbox_res {
        Ok(toolbox) => Ok(toolbox),
        Err(_) => {
            let toolbox_took = Instant::now() - start_time;
            let timeout = dur.saturating_sub(toolbox_took);
            crate::open_with_timeout_at::<P>(
                timeout,
                LEGACY_MONIKER,
                OpenDirType::NamespaceDir,
                &format!("svc/{protocol_name}"),
                rcs_proxy,
            )
            .await
        }
    };

    let toolbox_took = Instant::now() - start_time;

    let Some(backup) = backup_moniker.as_ref().map(|s| s.as_ref()) else {
        return toolbox_res.map_err(|e| ToolboxError::ProtocolNotFoundAtToolbox {
            protocol_name: protocol_name.to_string(),
            source: e,
        });
    };
    if let Ok(toolbox) = toolbox_res {
        return Ok(toolbox);
    }

    // try to connect to the moniker given instead, but don't double
    // up the timeout.
    let timeout = dur.saturating_sub(toolbox_took);
    let moniker_res =
        crate::open_with_timeout::<P>(timeout, &backup, OpenDirType::ExposedDir, &rcs_proxy).await;

    moniker_res.map_err(|e| ToolboxError::ProtocolNotFoundAtBackup {
        protocol_name: protocol_name.to_string(),
        backup_moniker: backup.to_string(),
        source: e,
    })
}
