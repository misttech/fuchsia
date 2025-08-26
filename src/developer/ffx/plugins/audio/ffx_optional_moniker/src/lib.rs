// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Provides a helper for `ffx` plugins to connect to FIDL protocols
//! that may or may not be available on the target device.
//!
//! This is particularly useful when a command needs to interact with a
//! component that is not guaranteed to be running. Instead of failing
//! with an error, the connection attempt will gracefully return `None`.
//!
//! # Usage
//!
//! This crate is intended to be used with the `fho` framework. Decorate a
//! tool field with `#[fho(from_env_with = ...)]` and provide one of the
//! constructor functions from this crate.
//!
//! ## Example
//!
//! ```rust
//! use fho::{FfxTool, FfxMain};
//! use fidl_fuchsia_my_protocol::MyProtocolProxy;
//! use ffx_optional_moniker::optional_moniker;
//!
//! #[derive(FfxTool)]
//! pub struct MyTool {
//!     #[fho(from_env_with = optional_moniker("/core/my_component"))]
//!     proxy: Option<MyProtocolProxy>,
//! }
//!
//! #[async_trait(?Send)]
//! impl FfxMain for MyTool {
//!     type Writer = ffx_writer::SimpleWriter;
//!     async fn main(self, writer: Self::Writer) -> fho::Result<()> {
//!         if let Some(proxy) = self.proxy {
//!             writer.line("Successfully connected to MyProtocolProxy.")?;
//!             // ... use the proxy
//!         } else {
//!             writer.line("Could not connect to MyProtocolProxy, but that's okay.")?;
//!         }
//!         Ok(())
//!     }
//! }
//! ```

use async_trait::async_trait;
use ffx_command_error::Result;
use fho::{FhoEnvironment, TryFromEnv, TryFromEnvWith};
use fidl::endpoints::{DiscoverableProtocolMarker, Proxy};
use fidl_fuchsia_io as fio;
use std::marker::PhantomData;
use std::time::Duration;
use target_holders::RemoteControlProxyHolder;

const DEFAULT_PROXY_TIMEOUT: Duration = Duration::from_secs(15);

/// Connector for FfxTool fields that creates a DirectoryProxy
/// for a directory capability exposed by a component.
pub struct ExposedDirectoryConnector {
    moniker: String,
    capability_name: String,
    timeout: Duration,
}

impl ExposedDirectoryConnector {
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[async_trait(?Send)]
impl TryFromEnvWith for ExposedDirectoryConnector {
    type Output = fio::DirectoryProxy;

    async fn try_from_env_with(self, env: &FhoEnvironment) -> Result<Self::Output> {
        // It would be better to use connect_to_rcs that retries, but it's private.
        let rcs = RemoteControlProxyHolder::try_from_env(env).await?;
        let proxy = rcs::open_with_timeout_at::<fio::DirectoryMarker>(
            self.timeout,
            &self.moniker,
            rcs::OpenDirType::ExposedDir,
            &self.capability_name,
            &rcs,
        )
        .await?;
        Ok(proxy)
    }
}

/// Connects to the directory capability exposed by the component at `moniker`.
pub fn exposed_dir(
    moniker: impl Into<String>,
    capability_name: impl Into<String>,
) -> ExposedDirectoryConnector {
    ExposedDirectoryConnector {
        moniker: moniker.into(),
        capability_name: capability_name.into(),
        timeout: DEFAULT_PROXY_TIMEOUT,
    }
}

/// The implementation of the decorator returned by [`optional_moniker`].
pub struct OptionalProtocolConnector<P> {
    backup: Option<String>,
    timeout: Duration,
    _p: PhantomData<fn() -> P>,
}

impl<P> OptionalProtocolConnector<P> {
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[async_trait(?Send)]
impl<P> TryFromEnvWith for OptionalProtocolConnector<P>
where
    P: Proxy + 'static,
    P::Protocol: DiscoverableProtocolMarker,
{
    type Output = Option<P>;

    async fn try_from_env_with(self, env: &FhoEnvironment) -> Result<Self::Output> {
        // It would be better to use connect_to_rcs that retries, but it's private.
        let rcs = RemoteControlProxyHolder::try_from_env(env).await?;
        let output = match rcs::toolbox::connect_with_timeout::<P::Protocol>(
            &rcs,
            self.backup.as_ref(),
            self.timeout,
        )
        .await
        {
            Ok(proxy) => Some(proxy),
            Err(err) => {
                log::debug!("Protocol {} is unavailable. err: {}", P::Protocol::PROTOCOL_NAME, err);
                None
            }
        };
        Ok(output)
    }
}

/// Connects to an optional protocol that may be exposed by the toolbox
/// or the component with the given moniker.
///
/// Essentially, this is the optional version of `fho::moniker`.
///
/// If the component with the moniker does not exist or fails to connect,
/// the field is set to None.
pub fn optional_moniker<P: Proxy>(or_moniker: impl Into<String>) -> OptionalProtocolConnector<P> {
    OptionalProtocolConnector {
        backup: Some(or_moniker.into()),
        timeout: DEFAULT_PROXY_TIMEOUT,
        _p: PhantomData {},
    }
}
