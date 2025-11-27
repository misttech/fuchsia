// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fidl::registry::try_from_handle_in_registry;
use crate::{Capability, ConversionError, RemotableCapability, RemoteError, WeakInstanceToken};
use fidl::AsHandleRef;
use fidl_fuchsia_component_sandbox as fsandbox;
use std::sync::Arc;
use vfs::directory::entry::DirectoryEntry;
use vfs::execution_scope::ExecutionScope;

impl crate::fidl::IntoFsandboxCapability for Capability {
    fn into_fsandbox_capability(self, token: WeakInstanceToken) -> fsandbox::Capability {
        match self {
            Capability::Connector(s) => s.into_fsandbox_capability(token),
            Capability::DirConnector(s) => s.into_fsandbox_capability(token),
            Capability::DirEntry(s) => s.into_fsandbox_capability(token),
            Capability::DictionaryRouter(s) => s.into_fsandbox_capability(token),
            Capability::ConnectorRouter(s) => s.into_fsandbox_capability(token),
            Capability::DirEntryRouter(s) => s.into_fsandbox_capability(token),
            Capability::DirConnectorRouter(s) => s.into_fsandbox_capability(token),
            Capability::DataRouter(s) => s.into_fsandbox_capability(token),
            Capability::Dictionary(s) => s.into_fsandbox_capability(token),
            Capability::Data(s) => s.into_fsandbox_capability(token),
            Capability::Unit(s) => s.into_fsandbox_capability(token),
            Capability::Directory(s) => s.into_fsandbox_capability(token),
            Capability::Handle(s) => s.into_fsandbox_capability(token),
            Capability::Instance(s) => s.into_fsandbox_capability(token),
        }
    }
}

impl TryFrom<fsandbox::Capability> for Capability {
    type Error = RemoteError;

    /// Converts the FIDL capability back to a Rust Capability.
    ///
    /// In most cases, the Capability was previously inserted into the registry when it
    /// was converted to a FIDL capability. This method takes it out of the registry.
    fn try_from(capability: fsandbox::Capability) -> Result<Self, Self::Error> {
        match capability {
            fsandbox::Capability::Unit(_) => Ok(crate::Unit::default().into()),
            fsandbox::Capability::Handle(handle) => Ok(crate::Handle::new(handle).into()),
            fsandbox::Capability::Data(data_capability) => {
                Ok(crate::Data::try_from(data_capability)?.into())
            }
            fsandbox::Capability::Dictionary(dict) => Ok(crate::Dict::try_from(dict)?.into()),
            fsandbox::Capability::Connector(connector) => {
                let any = try_from_handle_in_registry(connector.token.as_handle_ref())?;
                match &any {
                    Capability::Connector(_) => (),
                    _ => return Err(RemoteError::BadCapability),
                };
                Ok(any)
            }
            fsandbox::Capability::DirConnector(connector) => {
                let any = try_from_handle_in_registry(connector.token.as_handle_ref())?;
                match &any {
                    Capability::DirConnector(_) => (),
                    _ => return Err(RemoteError::BadCapability),
                };
                Ok(any)
            }
            fsandbox::Capability::Directory(client_end) => {
                Ok(crate::Directory::new(client_end).into())
            }
            fsandbox::Capability::ConnectorRouter(client_end) => {
                let any = try_from_handle_in_registry(client_end.as_handle_ref())?;
                match &any {
                    Capability::ConnectorRouter(_) => (),
                    _ => return Err(RemoteError::BadCapability),
                };
                Ok(any)
            }
            fsandbox::Capability::DictionaryRouter(client_end) => {
                let any = try_from_handle_in_registry(client_end.as_handle_ref())?;
                match &any {
                    Capability::DictionaryRouter(_) => (),
                    _ => return Err(RemoteError::BadCapability),
                };
                Ok(any)
            }
            fsandbox::Capability::DirEntryRouter(client_end) => {
                let any = try_from_handle_in_registry(client_end.as_handle_ref())?;
                match &any {
                    Capability::DirEntryRouter(_) => (),
                    _ => return Err(RemoteError::BadCapability),
                };
                Ok(any)
            }
            fsandbox::Capability::DataRouter(client_end) => {
                let any = try_from_handle_in_registry(client_end.as_handle_ref())?;
                match &any {
                    Capability::DataRouter(_) => (),
                    _ => return Err(RemoteError::BadCapability),
                };
                Ok(any)
            }
            fsandbox::Capability::DirEntry(dir_entry) => {
                let any = try_from_handle_in_registry(dir_entry.token.as_handle_ref())?;
                match &any {
                    Capability::DirEntry(_) => (),
                    _ => return Err(RemoteError::BadCapability),
                };
                Ok(any)
            }
            fsandbox::CapabilityUnknown!() => Err(RemoteError::UnknownVariant),
        }
    }
}

impl RemotableCapability for Capability {
    fn try_into_directory_entry(
        self,
        scope: ExecutionScope,
        token: WeakInstanceToken,
    ) -> Result<Arc<dyn DirectoryEntry>, ConversionError> {
        match self {
            Self::Connector(s) => s.try_into_directory_entry(scope, token),
            Self::DirConnector(s) => s.try_into_directory_entry(scope, token),
            Self::DirEntry(s) => s.try_into_directory_entry(scope, token),
            Self::ConnectorRouter(s) => s.try_into_directory_entry(scope, token),
            Self::DictionaryRouter(s) => s.try_into_directory_entry(scope, token),
            Self::DirEntryRouter(s) => s.try_into_directory_entry(scope, token),
            Self::DirConnectorRouter(s) => s.try_into_directory_entry(scope, token),
            Self::DataRouter(s) => s.try_into_directory_entry(scope, token),
            Self::Dictionary(s) => s.try_into_directory_entry(scope, token),
            Self::Data(s) => s.try_into_directory_entry(scope, token),
            Self::Unit(s) => s.try_into_directory_entry(scope, token),
            Self::Directory(s) => s.try_into_directory_entry(scope, token),
            Self::Handle(s) => s.try_into_directory_entry(scope, token),
            Self::Instance(s) => s.try_into_directory_entry(scope, token),
        }
    }
}
