// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod availability;
pub mod bedrock;
pub mod component_instance;
pub mod config;
pub mod error;
pub mod policy;
pub mod resolving;
pub mod rights;
pub mod subdir;

use crate::bedrock::request_metadata::directory_metadata;
use crate::component_instance::{ComponentInstanceInterface, ResolvedInstanceInterface};
use crate::error::RoutingError;
use capability_source::CapabilitySource;
use cm_rust::{
    Availability, ExposeDecl, ExposeDeclCommon, ExposeTarget, OfferDecl, OfferDeclCommon,
    OfferTarget, StorageDecl, StorageDirectorySource, UseDecl,
};
use cm_types::{IterablePath, Name, RelativePath};
use fidl_fuchsia_component_runtime::RouteRequest;
use fidl_fuchsia_io::RW_STAR_DIR;
use itertools::Itertools;
use moniker::{ChildName, ExtendedMoniker};
use runtime_capabilities::{
    Capability, CapabilityBound, Dictionary, DirConnector, Routable, Router,
};
use std::fmt::Debug;
use std::sync::Arc;

pub use bedrock::dict_ext::{DictExt, GenericRouterResponse};
pub use bedrock::lazy_get::LazyGet;
pub use bedrock::weak_instance_token_ext::{WeakInstanceTokenExt, test_invalid_instance_token};
pub use bedrock::with_porcelain::WithPorcelain;

#[derive(Clone)]
pub struct SandboxPath {
    path: String,
}

impl SandboxPath {
    pub fn resolver(scheme: &str) -> Self {
        Self { path: format!("component_input/environment/resolvers/{}", &scheme) }
    }

    pub fn used_path(target_path: &impl IterablePath) -> Self {
        let path: RelativePath = target_path.iter_segments().collect::<Vec<_>>().into();
        Self { path: format!("program_input/namespace/{}", path) }
    }
}

impl From<&UseDecl> for SandboxPath {
    fn from(use_decl: &UseDecl) -> Self {
        let path = match use_decl {
            UseDecl::Config(u) => format!("program_input/config/{}", u.target_name),
            UseDecl::Dictionary(u) => format!("program_input/namespace{}", u.target_path),
            UseDecl::Directory(u) => format!("program_input/namespace{}", u.target_path),
            UseDecl::EventStream(u) => format!("program_input/namespace{}", u.target_path),
            UseDecl::Protocol(u) => match (&u.target_path, &u.numbered_handle) {
                (Some(target_path), None) => format!("program_input/namespace{}", target_path),
                (None, Some(numbered_handle)) => {
                    format!("program_input/numbered_handles/{}", Name::from(*numbered_handle))
                }
                _ => panic!("invalid use decl"),
            },
            UseDecl::Runner(_u) => "program_input/runner".to_string(),
            UseDecl::Service(u) => format!("program_input/namespace{}", u.target_path),
            UseDecl::Storage(u) => format!("program_input/namespace{}", u.target_path),
        };
        Self { path }
    }
}

impl From<&OfferDecl> for SandboxPath {
    fn from(offer_decl: &OfferDecl) -> Self {
        let path = match offer_decl.target() {
            OfferTarget::Child(child_ref) if child_ref.collection.is_some() => {
                panic!("dynamic offers not supported")
            }
            OfferTarget::Child(child_ref) => {
                format!("child_inputs/{}/parent/{}", child_ref.name, offer_decl.target_name())
            }
            OfferTarget::Collection(name) => {
                format!("collection_inputs/{}/parent/{}", name, offer_decl.target_name())
            }
            OfferTarget::Capability(name) => {
                format!("declared_dictionaries/{}/{}", name, offer_decl.target_name())
            }
        };
        Self { path }
    }
}

impl From<&ExposeDecl> for SandboxPath {
    fn from(expose_decl: &ExposeDecl) -> Self {
        let path = match expose_decl.target() {
            ExposeTarget::Parent => {
                format!("component_output/parent/{}", expose_decl.target_name())
            }
            ExposeTarget::Framework => {
                format!("component_output/framework/{}", expose_decl.target_name())
            }
        };
        Self { path }
    }
}

impl From<SandboxPath> for RelativePath {
    fn from(path: SandboxPath) -> Self {
        RelativePath::new(&path.path).expect("invalid path string")
    }
}

/// Calls `route` on the router at the given path within the component sandbox. Panics if the
/// sandbox does not hold a router at that path.
pub async fn debug_route_sandbox_path<C: ComponentInstanceInterface + 'static>(
    component: &Arc<C>,
    sandbox_path: impl Into<SandboxPath>,
) -> Result<CapabilitySource, RoutingError> {
    debug_route_sandbox_path_with_request(component, sandbox_path, RouteRequest::default()).await
}

/// Calls `route` on the router with the given request at the given path within the component
/// sandbox. Panics if the sandbox does not hold a router at that path.
pub async fn debug_route_sandbox_path_with_request<C: ComponentInstanceInterface + 'static>(
    component: &Arc<C>,
    sandbox_path: impl Into<SandboxPath>,
    request: RouteRequest,
) -> Result<CapabilitySource, RoutingError> {
    let sandbox_path = sandbox_path.into();
    let path: RelativePath = sandbox_path.clone().into();
    let sandbox = component.component_sandbox().await.map_err(RoutingError::from)?;
    let sandbox_dictionary: Dictionary = sandbox.into();
    let maybe_response = sandbox_dictionary
        .get_with_request(
            &ExtendedMoniker::ComponentManager,
            &path,
            request,
            true,
            component.as_weak().into(),
        )
        .await
        .map_err(|e| RoutingError::try_from(e).expect("invalid routing error"))?;
    match maybe_response {
        Some(GenericRouterResponse::Debug(data)) => {
            Ok(data.try_into().expect("failed to deserialize capability source"))
        }
        None => Err(RoutingError::BedrockNotPresentInDictionary {
            name: sandbox_path.path,
            moniker: component.moniker().clone().into(),
        }),
        other_value => {
            panic!("unexpected response to route: {other_value:?}")
        }
    }
}

/// Routes the backing directory for the storage declaration on the component.
pub async fn debug_route_storage_backing_directory<C: ComponentInstanceInterface + 'static>(
    component: &Arc<C>,
    storage_decl: StorageDecl,
) -> Result<CapabilitySource, RoutingError> {
    let component_sandbox = component.component_sandbox().await?;
    let source_dictionary = match storage_decl.source {
        StorageDirectorySource::Parent => component_sandbox.component_input.capabilities(),
        StorageDirectorySource::Self_ => component_sandbox.program_output_dict.clone(),
        StorageDirectorySource::Child(static_name) => {
            let child_name = ChildName::parse(static_name)
                .expect("invalid child name, this should be prevented by manifest validation");
            let child_component = component
                .lock_resolved_state()
                .await?
                .get_child(&child_name)
                .expect("resolver registration references nonexistent static child, this should be prevented by manifest validation");
            let child_sandbox = child_component.component_sandbox().await?;
            child_sandbox.component_output.capabilities().clone()
        }
    };
    route_capability_inner::<DirConnector, _>(
        &source_dictionary,
        &storage_decl.backing_dir,
        directory_metadata(Availability::Required, Some(RW_STAR_DIR.into()), None),
        component,
    )
    .await
}

async fn route_capability_inner<T, C>(
    dictionary: &Dictionary,
    path: &impl IterablePath,
    request: RouteRequest,
    target: &Arc<C>,
) -> Result<CapabilitySource, RoutingError>
where
    C: ComponentInstanceInterface + 'static,
    T: CapabilityBound + Debug,
    Router<T>: TryFrom<Capability>,
{
    let router = dictionary.get_router_or_not_found(
        path,
        RoutingError::BedrockNotPresentInDictionary {
            moniker: target.moniker().clone().into(),
            name: path.iter_segments().join("/"),
        },
    );
    perform_route::<T, C>(router, request, target).await
}

async fn perform_route<T, C>(
    router: impl Routable<T>,
    request: RouteRequest,
    target: &Arc<C>,
) -> Result<CapabilitySource, RoutingError>
where
    C: ComponentInstanceInterface + 'static,
    T: CapabilityBound + Debug,
    Router<T>: TryFrom<Capability>,
{
    let data = router
        .route_debug(request, target.as_weak().into())
        .await
        .map_err(|e| RoutingError::try_from(e).unwrap_or(RoutingError::UnexpectedError))?;
    Ok(data.try_into().unwrap())
}
