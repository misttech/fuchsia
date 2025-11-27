// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod capability;
mod connector;
mod connector_router;
mod data;
mod data_router;
pub(crate) mod dict;
mod dictionary_router;
mod dir_connector;
mod dir_connector_router;
mod dir_entry;
mod dir_entry_router;
mod directory;
mod handle;
mod instance_token;
pub(crate) mod receiver;
pub(crate) mod registry;
pub(crate) mod router;
pub(crate) mod store;
mod unit;

use crate::{ConversionError, WeakInstanceToken};
use fidl_fuchsia_component_sandbox as fsandbox;
use std::sync::Arc;
use vfs::directory::entry::DirectoryEntry;
use vfs::execution_scope::ExecutionScope;

pub trait IntoFsandboxCapability {
    fn into_fsandbox_capability(self, _token: WeakInstanceToken) -> fsandbox::Capability
    where
        Self: Sized;
}

/// The trait which remotes Capabilities, either by turning them into
/// FIDL or serving them in a VFS.
pub trait RemotableCapability: IntoFsandboxCapability + Sized {
    /// Attempt to convert `self` to a DirectoryEntry which can be served in a VFS. If routing
    /// needs to be performed, `token` should be the `WeakInstanceToken` used for the route.
    ///
    /// The default implementation always returns an error.
    fn try_into_directory_entry(
        self,
        _scope: ExecutionScope,
        _token: WeakInstanceToken,
    ) -> Result<Arc<dyn DirectoryEntry>, ConversionError> {
        Err(ConversionError::NotSupported)
    }
}
