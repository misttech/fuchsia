// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{DEFAULT_PROXY_TIMEOUT, connect_to_rcs};
use async_trait::async_trait;
use ffx_command_error::Result;
use fho::{FhoEnvironment, TryFromEnvWith};
use fidl::encoding::DefaultFuchsiaResourceDialect;
use fidl::endpoints::{DiscoverableProtocolMarker, Proxy};
use std::marker::PhantomData;

/// The implementation of the decorator returned by [`toolbox`] and
/// [`toolbox_or`].
pub struct WithToolbox<P, D> {
    pub(crate) _p: PhantomData<(fn() -> P, D)>,
}

#[async_trait(?Send)]
impl<P> TryFromEnvWith for WithToolbox<P, DefaultFuchsiaResourceDialect>
where
    P: Proxy + 'static,
    P::Protocol: DiscoverableProtocolMarker,
{
    type Output = P;
    type Error = ffx_command_error::Error;
    async fn try_from_env_with(self, env: &FhoEnvironment) -> Result<Self::Output> {
        // start off by connecting to rcs
        let rcs = connect_to_rcs(env).await?;
        let proxy =
            rcs::toolbox::connect_with_timeout::<P::Protocol>(&rcs, DEFAULT_PROXY_TIMEOUT).await?;
        Ok(proxy)
    }
}

/// Uses the `/toolbox` to find the given proxy.
pub fn toolbox<P: Proxy>() -> WithToolbox<P, DefaultFuchsiaResourceDialect> {
    WithToolbox { _p: PhantomData::default() }
}
