// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use errors::CapabilityProviderError;
use vfs::directory::entry::OpenRequest;
use vfs::execution_scope::ExecutionScope;

/// The server-side of a capability implements this trait.
/// Multiple `CapabilityProvider` objects can compose with one another for a single
/// capability request. For example, a `CapabilityProvider` can be interposed
/// between the primary `CapabilityProvider and the client for the purpose of
/// logging and testing. A `CapabilityProvider` is typically provided by a
/// corresponding `Hook` in response to the `CapabilityRouted` event.
/// A capability provider is used exactly once as a result of exactly one route.
#[async_trait]
pub trait CapabilityProvider: Send + Sync {
    /// Binds a server end of a zx::Channel to the provided capability.  If the capability is a
    /// directory, then `flags`, and `relative_path` will be propagated along to open
    /// the appropriate directory.
    async fn open(
        self: Box<Self>,
        scope: ExecutionScope,
        open_request: OpenRequest<'_>,
    ) -> Result<(), CapabilityProviderError>;
}
