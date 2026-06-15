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
    /// Converts this error into a user-facing command error (`Error::User`) if the underlying
    /// error represents a target resolution or connection failure that is actionable by the
    /// user (such as target not found or ambiguous target query). Otherwise, wraps it as an
    /// unexpected error (`Error::Unexpected`).
    pub fn into_command_error(self) -> ffx_command_error::Error {
        // TODO(b/523421855): Simplify this nested downcasting once FfxTargetCrateError
        // has a more unified representation of target resolution/connection errors.
        match self {
            Self::Target(err) => {
                let ffx_err: errors::FfxError = err.into();
                ffx_command_error::Error::User(anyhow::Error::new(ffx_err))
            }
            Self::Fallback(err) => match err.downcast::<target_errors::FfxTargetError>() {
                Ok(target_err) => {
                    let ffx_err: errors::FfxError = target_err.into();
                    ffx_command_error::Error::User(anyhow::Error::new(ffx_err))
                }
                Err(err) => match err.downcast::<errors::FfxError>() {
                    Ok(ffx_err) => ffx_command_error::Error::User(anyhow::Error::new(ffx_err)),
                    Err(err) => ffx_command_error::Error::Unexpected(err),
                },
            },
            other => ffx_command_error::Error::Unexpected(anyhow::Error::new(other)),
        }
    }
}

impl Into<ffx_command_error::Error> for FfxTargetCrateError {
    fn into(self) -> ffx_command_error::Error {
        self.into_command_error()
    }
}
