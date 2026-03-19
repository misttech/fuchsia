// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::capability::CapabilityProvider;
use crate::model::component::{ComponentInstance, ExtendedInstance, WeakComponentInstance};
use crate::model::routing::providers::{
    DefaultComponentCapabilityProvider, NamespaceCapabilityProvider,
};
use ::routing::RouteSource;
use ::routing::capability_source::{
    AnonymizedAggregateSource, CapabilitySource, ComponentSource, NamespaceSource,
};
use ::routing::component_instance::ComponentInstanceInterface;
use errors::{CapabilityProviderError, ModelError, OpenError};
use fidl_fuchsia_io as fio;
use routing::capability_source::StorageBackingDirectorySource;
use std::sync::Arc;
use vfs::directory::entry::OpenRequest;

/// A request to open a capability at its source.
pub enum CapabilityOpenRequest<'a> {
    // Open a capability backed by a component's outgoing directory.
    OutgoingDirectory {
        open_request: OpenRequest<'a>,
        source: Box<CapabilitySource>,
        target: &'a Arc<ComponentInstance>,
    },
}

impl<'a> CapabilityOpenRequest<'a> {
    #[allow(clippy::result_large_err)] // TODO(https://fxbug.dev/401254441)
    /// Creates a request to open a capability with source `route_source` for `target`.
    pub fn new_from_route_source(
        route_source: RouteSource,
        target: &'a Arc<ComponentInstance>,
        mut open_request: OpenRequest<'a>,
    ) -> Result<Self, OpenError> {
        let RouteSource { source, relative_path } = route_source;
        if !relative_path.is_dot() {
            open_request.prepend_path(
                &relative_path.to_string().try_into().map_err(|_| OpenError::BadPath)?,
            );
        }
        Ok(Self::OutgoingDirectory { open_request, source: Box::new(source), target })
    }

    /// Opens the capability in `self`, triggering a `CapabilityRouted` event and binding
    /// to the source component instance if necessary.
    pub async fn open(self) -> Result<(), OpenError> {
        let Self::OutgoingDirectory { open_request, source, target } = self;
        Self::open_outgoing_directory(open_request, *source, target).await
    }

    async fn open_outgoing_directory(
        mut open_request: OpenRequest<'a>,
        source: CapabilitySource,
        target: &Arc<ComponentInstance>,
    ) -> Result<(), OpenError> {
        let capability_provider = if let Some(provider) =
            Self::get_default_provider(target.as_weak(), &source)
                .await
                .map_err(|e| OpenError::GetDefaultProviderError { err: Box::new(e) })?
        {
            provider
        } else {
            return Err(OpenError::CapabilityProviderNotFound);
        };

        let source_instance = target
            .find_extended_instance(&source.source_moniker())
            .await
            .map_err(|err| CapabilityProviderError::ComponentInstanceError { err })?;
        let scope = match source_instance {
            ExtendedInstance::AboveRoot(top) => top.execution_scope().clone(),
            ExtendedInstance::Component(component) => {
                open_request.set_scope(component.execution_scope.clone());
                component.execution_scope.clone()
            }
        };
        capability_provider.open(scope, open_request).await?;
        Ok(())
    }

    /// Returns an instance of the default capability provider for the capability at `source`, if
    /// supported.
    async fn get_default_provider(
        target: WeakComponentInstance,
        source: &CapabilitySource,
    ) -> Result<Option<Box<dyn CapabilityProvider>>, ModelError> {
        match source {
            CapabilitySource::Component(ComponentSource { capability, moniker })
            | CapabilitySource::StorageBackingDirectory(StorageBackingDirectorySource {
                capability,
                moniker,
                ..
            }) => {
                // Route normally for a component capability with a source path
                Ok(match capability.source_path() {
                    Some(_) => Some(Box::new(DefaultComponentCapabilityProvider::new(
                        target,
                        moniker.clone(),
                        capability
                            .source_name()
                            .expect("capability with source path should have a name")
                            .clone(),
                    ))),
                    _ => None,
                })
            }
            CapabilitySource::Namespace(NamespaceSource { capability, .. }) => {
                match capability.source_path() {
                    Some(path) => Ok(Some(Box::new(NamespaceCapabilityProvider {
                        path: path.clone(),
                        is_directory_like: fio::DirentType::from(capability.type_name())
                            == fio::DirentType::Directory,
                    }))),
                    _ => Ok(None),
                }
            }
            CapabilitySource::FilteredProvider(_)
            | CapabilitySource::FilteredAggregateProvider(_)
            | CapabilitySource::AnonymizedAggregate(AnonymizedAggregateSource { .. }) => {
                // This function should only be used for legacy routing, and these capability
                // sources have been fully moved to bedrock routing.
                panic!("this code should never be reached");
            }
            // These capabilities do not have a default provider.
            CapabilitySource::Framework(_)
            | CapabilitySource::Void(_)
            | CapabilitySource::Capability(_)
            | CapabilitySource::Builtin(_)
            | CapabilitySource::Environment(_)
            | CapabilitySource::RemotedAt(_) => Ok(None),
        }
    }
}
