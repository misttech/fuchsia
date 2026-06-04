// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::net::SocketAddr;

#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum TargetResolutionError {
    #[error("Target {node_name:?} did not have a product address")]
    MissingProductAddress { node_name: Option<String> },

    #[error("Target does not connect via networking")]
    NonNetworkTarget,

    #[error("Network connections are disabled in configuration")]
    NetworkDisabled,

    #[error("USB connections are disabled in configuration")]
    UsbDisabled,

    #[error("VSOCK connections are disabled in configuration")]
    VsockDisabled,

    #[error("Timeout after {timeout:?} identifying manual target {addr}")]
    ManualTargetTimeout { addr: SocketAddr, timeout: std::time::Duration },

    #[error("Connection to target was terminated")]
    ConnectionTerminated,
}

#[derive(Debug, thiserror::Error)]
pub enum FfxTargetCrateError {
    #[error("Cache error: {0}")]
    Cache(#[from] crate::cache::CacheError),

    #[error("Connection error: {0}")]
    Connection(#[from] crate::connection::ConnectionError),

    #[error("Knock error: {0}")]
    Knock(#[from] crate::KnockError),

    #[error("Ffx configuration error: {0}")]
    Config(#[from] ffx_config::api::ConfigError),

    #[error("Socket address translation failed: {0}")]
    SocketAddr(#[from] std::net::AddrParseError),

    #[error("Discovery error: {0}")]
    Discovery(#[from] discovery::error::Error),

    #[error("Target error: {0}")]
    Target(#[from] target_errors::FfxTargetError),

    #[error("Target parsing error: {0}")]
    JsonParse(#[from] serde_json::Error),

    #[error("Target resolution error: {0}")]
    Resolution(#[from] TargetResolutionError),

    #[error("Identify host failed with error: {0:?}")]
    IdentifyHost(fdomain_fuchsia_developer_remotecontrol::IdentifyHostError),

    #[error("USB Driver error: {0}")]
    UsbDriver(#[from] usb_driver_api::ProtocolError),

    #[error("Invalid network interface ID: {0}")]
    InvalidInterfaceId(#[from] netext::InvalidInterfaceIdError),

    #[error("FIDL error: {0}")]
    Fidl(#[from] fidl::Error),

    #[error("Temporary fallback error: {0}")]
    Fallback(#[from] anyhow::Error),
}

impl FfxTargetCrateError {
    pub fn into_command_error(self) -> ffx_command_error::Error {
        ffx_command_error::Error::Unexpected(anyhow::Error::new(self))
    }
}

impl Into<ffx_command_error::Error> for FfxTargetCrateError {
    fn into(self) -> ffx_command_error::Error {
        self.into_command_error()
    }
}
