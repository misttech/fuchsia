// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use fdomain_client::fidl::DiscoverableProtocolMarker;
use ffx_command_error::Result;
use fho::{FhoEnvironment, TryFromEnvWith};
use fidl::encoding::DefaultFuchsiaResourceDialect;
use fidl::endpoints::Proxy;
use std::marker::PhantomData;
use std::time::Duration;

use crate::connect_to_rcs;

#[allow(dead_code)] // TODO(https://fxbug.dev/421409514)
/// The implementation of the decorator returned by [`moniker`].
pub struct WithMoniker<P, D> {
    moniker: String,
    timeout: Duration,
    _p: PhantomData<(fn() -> P, D)>,
}

#[async_trait(?Send)]
impl<P> TryFromEnvWith for WithMoniker<P, fdomain_client::fidl::FDomainResourceDialect>
where
    P: fdomain_client::fidl::Proxy + 'static,
    P::Protocol: fdomain_client::fidl::DiscoverableProtocolMarker,
{
    type Output = P;
    async fn try_from_env_with(self, env: &FhoEnvironment) -> Result<Self::Output> {
        let rcs_instance = crate::fdomain::connect_to_rcs(&env).await?;
        crate::fdomain::open_moniker_fdomain(
            &rcs_instance,
            rcs_fdomain::OpenDirType::ExposedDir,
            &self.moniker,
            self.timeout,
        )
        .await
    }
}

#[async_trait(?Send)]
impl<P> TryFromEnvWith for WithMoniker<P, DefaultFuchsiaResourceDialect>
where
    P: Proxy + 'static,
    P::Protocol: fidl::endpoints::DiscoverableProtocolMarker,
{
    type Output = P;
    async fn try_from_env_with(self, env: &FhoEnvironment) -> Result<Self::Output> {
        let rcs_instance = connect_to_rcs(&env).await?;
        crate::remote_control_proxy::open_moniker(
            &rcs_instance,
            rcs::OpenDirType::ExposedDir,
            &self.moniker,
            self.timeout,
        )
        .await
    }
}

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
    type Output = flex_fuchsia_io::DirectoryProxy;

    async fn try_from_env_with(self, env: &FhoEnvironment) -> Result<Self::Output> {
        let rcs = crate::fdomain::connect_to_rcs(env).await?;
        let proxy = rcs_fdomain::open_with_timeout_at::<flex_fuchsia_io::DirectoryMarker>(
            self.timeout,
            &self.moniker,
            rcs_fdomain::OpenDirType::ExposedDir,
            &self.capability_name,
            &rcs,
        )
        .await
        .map_err(|e| fho::bug!(e))?;
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
    P: fdomain_client::fidl::Proxy + 'static,
    P::Protocol: fdomain_client::fidl::DiscoverableProtocolMarker,
{
    type Output = Option<P>;

    async fn try_from_env_with(self, env: &FhoEnvironment) -> Result<Self::Output> {
        let rcs = crate::fdomain::connect_to_rcs(env).await?;
        let output = match rcs_fdomain::toolbox::connect_with_timeout::<P::Protocol>(
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
pub fn optional_moniker<P: fdomain_client::fidl::Proxy>(
    or_moniker: impl Into<String>,
) -> OptionalProtocolConnector<P> {
    OptionalProtocolConnector {
        backup: Some(or_moniker.into()),
        timeout: DEFAULT_PROXY_TIMEOUT,
        _p: PhantomData {},
    }
}
