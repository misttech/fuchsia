// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::error::{ComponentInstanceError, RouteVerb, RoutingError};
use crate::rights::{Rights, validate_rights};
use async_trait::async_trait;
use capability_source::CapabilitySource;
use cm_rust::{CapabilityTypeName, FidlIntoNative, GenericRef, NativeIntoFidl};
use cm_rust_derive as _;
use cm_types::RelativePath;
use fidl_fuchsia_component_runtime as fruntime;
use fidl_fuchsia_io as fio;
use moniker::{ChildName, Moniker};
use router_error::RouterError;
use runtime_capabilities::{
    Capability, CapabilityBound, Connector, Data, Dictionary, DirConnector, Routable, Router,
    RouterErrorInfo, WeakInstanceToken,
};
use std::sync::{Arc, Weak};

#[cfg(target_os = "fuchsia")]
use {cm_types::IterablePath, fuchsia_trace as trace, itertools::Itertools};

pub enum WeakDictionaryOrRouter {
    Dictionary(Weak<Dictionary>),
    Router(Weak<Router<Dictionary>>),
}

impl From<Weak<Dictionary>> for WeakDictionaryOrRouter {
    fn from(d: Weak<Dictionary>) -> Self {
        Self::Dictionary(d)
    }
}

impl From<Weak<Router<Dictionary>>> for WeakDictionaryOrRouter {
    fn from(r: Weak<Router<Dictionary>>) -> Self {
        Self::Router(r)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteRequest {
    pub build_type_name: cm_rust::CapabilityTypeName,
    pub availability: Option<cm_rust::Availability>,
    pub directory_rights: Option<fio::Flags>,
    pub sub_directory_path: Option<RelativePath>,
    pub event_stream_scope_moniker: Option<Moniker>,
    pub event_stream_scope: Option<Box<[cm_rust::EventScope]>>,
}

impl Default for RouteRequest {
    fn default() -> Self {
        Self {
            // Having Default implemented reduces verbosity in places where we assemble this
            // struct, but it's also more convenient to not have `build_type_name` wrapped in an
            // `Option`. We set the default to `Protocol` here, but anyone using this should
            // overwrite that.
            build_type_name: cm_rust::CapabilityTypeName::Protocol,
            availability: None,
            directory_rights: None,
            sub_directory_path: None,
            event_stream_scope_moniker: None,
            event_stream_scope: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TypeSpecificProperties {
    // storage and directory
    DirectoryType(Box<DirectoryProperties>),
    // event stream
    EventStream(Box<EventStreamProperties>),
    // everything else
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirectoryRights {
    Explicit(fio::Flags),
    Inherit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DirectoryProperties {
    rights: DirectoryRights,
    // default to "."
    subdir: RelativePath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EventStreamProperties {
    moniker: Moniker,
    scope: Box<[cm_rust::EventScope]>,
}

/// A router that attempts to find a router to forward to in a source dictionary, validates (and
/// potentially mutates) the route request, and then forwards the request to next router.
///
/// Because `component_manager` retains thousands of `IntermediateRouter` instances across the
/// component topology, type-specific fields of `RouteRequest` are boxed in
/// `TypeSpecificProperties` so that each `IntermediateRouter` occupies 88 bytes instead of 216
/// bytes.
pub struct IntermediateRouter {
    /// The dictionary (or router for a dictionary) which will hold the router that we forward to.
    source_dictionary: WeakDictionaryOrRouter,

    /// The path in the source dictionary at which we should find the router to forward to.
    source_path: RelativePath,

    /// The "default" instance token to use when performing routes in order to find the router we
    /// will forward the request to.
    default_token: Arc<WeakInstanceToken>,

    /// The moniker of the component which created this router.
    moniker: Moniker,

    /// The source reference of the route.
    source: GenericRef,

    /// Type-specific routing properties boxed for directory/storage and scoped event streams.
    type_specific: TypeSpecificProperties,

    type_name: CapabilityTypeName,
    availability: Option<cm_rust::Availability>,
    verb: RouteVerb,

    /// Whether or not to emit tracing events for routes. This field is ignored during host
    /// routing, where tracing events are never emitted.
    #[allow(unused)]
    enable_tracing: bool,
}

// `component_manager` retains thousands of `IntermediateRouter` instances across the topology.
// Guard against accidental struct size regressions; type-specific fields belong in
// `TypeSpecificProperties`.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::size_of::<IntermediateRouter>() == 88);
    assert!(std::mem::size_of::<RouteRequest>() <= 72);
};

enum RouterCapabilityOrSource<C: CapabilityBound> {
    Router(Arc<Router<C>>),
    Capability(Arc<C>),
    Source(Box<CapabilitySource>),
}

impl IntermediateRouter {
    /// Returns an IntermediateRouter wrapped in a Router type, with an appropriate type for the
    /// `default_request.build_type_name`.
    pub fn new(
        source_dictionary: WeakDictionaryOrRouter,
        source_path: RelativePath,
        default_request: RouteRequest,
        default_token: Arc<WeakInstanceToken>,
        moniker: Moniker,
        route_verb: RouteVerb,
        source: GenericRef,
    ) -> Capability {
        assert!(source_path.len() != 0);
        let RouteRequest {
            build_type_name: type_name,
            availability,
            directory_rights,
            sub_directory_path,
            event_stream_scope_moniker,
            event_stream_scope,
        } = default_request;

        let type_specific = match type_name {
            CapabilityTypeName::Directory | CapabilityTypeName::Storage => {
                let rights = match directory_rights {
                    Some(rights) => DirectoryRights::Explicit(rights),
                    None => DirectoryRights::Inherit,
                };
                let subdir = sub_directory_path.unwrap_or_else(RelativePath::dot);
                TypeSpecificProperties::DirectoryType(Box::new(DirectoryProperties {
                    rights,
                    subdir,
                }))
            }
            CapabilityTypeName::EventStream => {
                match (event_stream_scope_moniker, event_stream_scope) {
                    (Some(moniker), Some(scope)) => TypeSpecificProperties::EventStream(Box::new(
                        EventStreamProperties { moniker, scope },
                    )),
                    (None, None) => TypeSpecificProperties::None,
                    _ => panic!(
                        "event_stream_scope_moniker and event_stream_scope must both be Some or both be None"
                    ),
                }
            }
            CapabilityTypeName::Protocol
            | CapabilityTypeName::Service
            | CapabilityTypeName::Config
            | CapabilityTypeName::Runner
            | CapabilityTypeName::Resolver
            | CapabilityTypeName::Dictionary => TypeSpecificProperties::None,
        };

        let enable_tracing = route_verb == RouteVerb::Use;

        let self_ = Self {
            source_dictionary,
            source_path,
            default_token,
            moniker,
            source,
            type_specific,
            type_name,
            availability,
            verb: route_verb,
            enable_tracing,
        };

        match type_name {
            CapabilityTypeName::Protocol
            | CapabilityTypeName::Runner
            | CapabilityTypeName::Resolver => Router::<Connector>::new(self_).into(),
            CapabilityTypeName::Service
            | CapabilityTypeName::Directory
            | CapabilityTypeName::Storage => Router::<DirConnector>::new(self_).into(),
            CapabilityTypeName::EventStream | CapabilityTypeName::Dictionary => {
                Router::<Dictionary>::new(self_).into()
            }
            CapabilityTypeName::Config => Router::<Data>::new(self_).into(),
        }
    }

    fn default_directory_rights(&self) -> Option<fio::Flags> {
        match &self.type_specific {
            TypeSpecificProperties::DirectoryType(props) => match props.rights {
                DirectoryRights::Explicit(rights) => Some(rights),
                DirectoryRights::Inherit => None,
            },
            _ if self.type_name == CapabilityTypeName::Service => Some(fio::PERM_READABLE),
            _ => None,
        }
    }

    fn default_inherit_rights(&self) -> Option<bool> {
        match &self.type_specific {
            TypeSpecificProperties::DirectoryType(props) => match props.rights {
                DirectoryRights::Inherit => Some(true),
                DirectoryRights::Explicit(_) => Some(false),
            },
            _ if self.type_name == CapabilityTypeName::Service => Some(false),
            _ => None,
        }
    }

    fn default_sub_directory_path(&self) -> Option<RelativePath> {
        match &self.type_specific {
            TypeSpecificProperties::DirectoryType(props) => Some(props.subdir.clone()),
            _ => None,
        }
    }

    fn to_not_found_router_error(&self) -> RouterError {
        RoutingError::RouteSourceNotFound {
            moniker: self.moniker.clone(),
            verb: self.verb,
            counter_verb: match &self.source {
                GenericRef::Parent => RouteVerb::Offer,
                GenericRef::Child(_)
                | GenericRef::ChildInCollection(_, _)
                | GenericRef::Collection(_) => RouteVerb::Expose,
                GenericRef::Self_ | GenericRef::Capability(_) => RouteVerb::Declare,
                GenericRef::Framework
                | GenericRef::Debug
                | GenericRef::Void
                | GenericRef::Environment => RouteVerb::Contain,
            },
            source: self.source.clone(),
            capability_type: self.type_name,
            capability_name: self.source_path.clone(),
        }
        .into()
    }

    fn default_request_to_fidl(&self) -> fruntime::RouteRequest {
        let (event_stream_scope_moniker, event_stream_scope) = match &self.type_specific {
            TypeSpecificProperties::EventStream(props) => (
                Some(props.moniker.clone().native_into_fidl()),
                Some(props.scope.clone().native_into_fidl()),
            ),
            _ => (None, None),
        };
        fruntime::RouteRequest {
            build_type_name: Some(self.type_name.native_into_fidl()),
            availability: self.availability.map(NativeIntoFidl::native_into_fidl),
            directory_rights: self.default_directory_rights(),
            inherit_rights: self.default_inherit_rights(),
            sub_directory_path: self
                .default_sub_directory_path()
                .map(NativeIntoFidl::native_into_fidl),
            event_stream_scope_moniker,
            event_stream_scope,
            ..Default::default()
        }
    }

    /// Returns the moniker that owns the source dictionary.
    fn get_upgrade_failure_moniker(&self) -> Moniker {
        match &self.source {
            GenericRef::Parent => self.moniker.parent().unwrap_or_else(|| Moniker::root()),

            GenericRef::Child(name) => {
                self.moniker.child(ChildName::new(name.clone().into(), None))
            }
            GenericRef::ChildInCollection(name, collection) => {
                self.moniker.child(ChildName::new(name.clone(), Some(collection.clone())))
            }

            GenericRef::Collection(_)
            | GenericRef::Capability(_)
            | GenericRef::Debug
            | GenericRef::Environment
            | GenericRef::Framework
            | GenericRef::Self_
            | GenericRef::Void => self.moniker.clone(),
        }
    }

    /// Upgrades and returns the dictionary which holds the router we will forward the request to.
    /// Initiates a routing operation for that dictionary if necessary.
    async fn upgrade_source_dictionary(
        &self,
        dictionary_request: &fruntime::RouteRequest,
    ) -> Result<Arc<Dictionary>, RouterError> {
        match &self.source_dictionary {
            WeakDictionaryOrRouter::Dictionary(dictionary) => dictionary.upgrade().ok_or(
                RoutingError::from(ComponentInstanceError::InstanceNotFound {
                    moniker: self.get_upgrade_failure_moniker(),
                })
                .into(),
            ),
            WeakDictionaryOrRouter::Router(router) => {
                let router = router.upgrade().ok_or(RoutingError::from(
                    ComponentInstanceError::InstanceNotFound {
                        moniker: self.get_upgrade_failure_moniker(),
                    },
                ))?;
                let dictionary = router
                    .route(dictionary_request.clone(), self.default_token.clone())
                    .await?
                    .expect("routers for source dictionaries should never return None");
                Ok(dictionary)
            }
        }
    }

    /// Upgrades self.source_dictionary and attempts to find a router at self.source_path in it of
    /// type Arc<Router<C>>. It does this by walking self.source_path, stepping down into each
    /// successive dictionary (routing dictionary routers as needed).
    async fn get_source_router<C: CapabilityBound>(
        &self,
        request: &fruntime::RouteRequest,
        debug: bool,
    ) -> Result<RouterCapabilityOrSource<C>, RouterError>
    where
        Arc<Router<C>>: TryFrom<Capability>,
        Arc<C>: TryFrom<Capability>,
        Router<C>: CapabilityBound,
    {
        let dictionary_request = fruntime::RouteRequest {
            build_type_name: Some(CapabilityTypeName::Dictionary.to_string()),
            ..request.clone()
        };
        // Get the dictionary holding the source router (if the dictionary still exists).
        let mut source_dictionary = self.upgrade_source_dictionary(&dictionary_request).await?;

        // Get the source router from the dictionary
        let mut path_to_walk = self.source_path.clone();
        let mut most_recent_source = None;
        while path_to_walk.len() > 1 {
            let next_step = path_to_walk.pop_front().expect("we checked that this isn't empty");
            match source_dictionary.get(&next_step) {
                Some(Capability::Dictionary(d)) => source_dictionary = d,
                Some(Capability::DictionaryRouter(r)) => {
                    match r.route(dictionary_request.clone(), self.default_token.clone()).await? {
                        Some(d) => {
                            if debug {
                                most_recent_source = Some(
                                    r.route_debug(
                                        dictionary_request.clone(),
                                        self.default_token.clone(),
                                    )
                                    .await?,
                                );
                            }
                            source_dictionary = d;
                        }
                        None => {
                            // The next step along our path is unavailable! If this is a debug
                            // route, we'll want the source of this unavailable dictionary.
                            let source = r
                                .route_debug(dictionary_request.clone(), self.default_token.clone())
                                .await?;
                            return Ok(RouterCapabilityOrSource::Source(Box::new(source)));
                        }
                    }
                }
                Some(capability) => {
                    return Err(RoutingError::BedrockWrongCapabilityType {
                        actual: capability.debug_typename().to_string(),
                        expected: Dictionary::debug_typename().to_string(),
                        moniker: self.moniker.clone().into(),
                    }
                    .into());
                }
                None => {
                    return Err(self.to_not_found_router_error());
                }
            }
        }
        let capability_name = path_to_walk
            .pop_front()
            .expect("we stopped the above loop before fully draining the path");
        let maybe_source_router = source_dictionary
            .get(&capability_name)
            .ok_or_else(|| self.to_not_found_router_error())?;

        let maybe_c: Option<Arc<C>> = maybe_source_router.clone().try_into().ok();
        if let Some(c) = maybe_c {
            if !debug {
                return Ok(RouterCapabilityOrSource::Capability(c));
            } else {
                let source = most_recent_source.ok_or_else(|| RoutingError::SourceUnknown {
                    capability_id: capability_name.to_string(),
                    moniker: self.moniker.clone().into(),
                })?;
                return Ok(RouterCapabilityOrSource::Source(Box::new(source)));
            }
        }

        let capability_type_name = maybe_source_router.debug_typename();
        let router: Arc<Router<C>> = maybe_source_router.try_into().map_err(|_| {
            RoutingError::BedrockWrongCapabilityType {
                actual: capability_type_name.to_string(),
                expected: Router::<C>::debug_typename().to_string(),
                moniker: self.moniker.clone().into(),
            }
        })?;
        Ok(RouterCapabilityOrSource::Router(router))
    }

    /// Check to see if `request` does not request anything different or larger in scope than
    /// `self`'s default request. `request` will be set to the default request if it is empty.
    fn handle_new_request(&self, request: &mut fruntime::RouteRequest) -> Result<(), RouterError> {
        if *request == fruntime::RouteRequest::default() {
            *request = self.default_request_to_fidl();
            return Ok(());
        }

        self.check_build_type_name(request)?;
        self.handle_availability(request)?;
        self.handle_directory_rights(request)?;
        self.handle_sub_directory(request)?;
        self.handle_event_stream_scope(request);

        Ok(())
    }

    fn check_build_type_name(
        &self,
        request: &mut fruntime::RouteRequest,
    ) -> Result<(), RouterError> {
        if request.build_type_name != Some(self.type_name.to_string()) {
            Err(RoutingError::BedrockWrongCapabilityType {
                moniker: self.moniker.clone().into(),
                actual: request
                    .build_type_name
                    .as_ref()
                    .map(Clone::clone)
                    .unwrap_or_else(|| "".to_string()),
                expected: self.type_name.to_string(),
            })?;
        }
        Ok(())
    }

    fn handle_availability(&self, request: &mut fruntime::RouteRequest) -> Result<(), RouterError> {
        let Some(default_availability) = self.availability else {
            return Ok(());
        };
        let request_availability: fidl_fuchsia_component_decl::Availability = *request
            .availability
            .as_ref()
            .ok_or_else(|| RoutingError::RouteRequestMissingField {
                moniker: self.moniker.clone().into(),
                missing_field: "availability".to_string(),
            })?;
        let request_availability: cm_rust::Availability = request_availability.fidl_into_native();
        let new_availability = crate::availability::advance(
            &self.moniker.clone().into(),
            request_availability,
            default_availability,
        )
        .map_err(|e| RoutingError::from(e))?;
        request.availability = Some(new_availability.native_into_fidl());
        Ok(())
    }

    fn handle_directory_rights(
        &self,
        request: &mut fruntime::RouteRequest,
    ) -> Result<(), RouterError> {
        let Some(directory_rights) = self.default_directory_rights() else {
            return Ok(());
        };
        let rights = Rights::from(directory_rights);
        validate_rights(self.moniker.clone().into(), rights.into(), request)?;
        request.directory_intermediate_rights = Some(fio::Flags::from(rights));
        Ok(())
    }

    fn handle_sub_directory(
        &self,
        request: &mut fruntime::RouteRequest,
    ) -> Result<(), RouterError> {
        let Some(mut new_subdir) = self.default_sub_directory_path() else {
            return Ok(());
        };

        let Some(current_subdir) = request.sub_directory_path.as_ref() else {
            request.sub_directory_path = Some(new_subdir.to_string());
            return Ok(());
        };
        let current_subdir = RelativePath::new(current_subdir).map_err(|e| {
            RoutingError::RouteRequestFailedToParseField {
                moniker: self.moniker.clone().into(),
                field: "sub_directory_path".to_string(),
                parse_error: format!("{e:?}"),
            }
        })?;

        let success = new_subdir.extend(current_subdir);
        if !success {
            return Err(RoutingError::PathTooLong {
                moniker: self.moniker.clone().into(),
                path: self.default_sub_directory_path().unwrap().to_string(),
                keyword: request.sub_directory_path.clone().unwrap(),
            }
            .into());
        }

        request.sub_directory_path = Some(new_subdir.native_into_fidl());
        Ok(())
    }

    fn handle_event_stream_scope(&self, request: &mut fruntime::RouteRequest) {
        if request.event_stream_scope_moniker.is_some() {
            // If the scope is already set then it's a smaller scope (because we can't expose
            // these), so only set our scope if the request doesn't have one yet.
            return;
        }
        if let TypeSpecificProperties::EventStream(props) = &self.type_specific {
            request.event_stream_scope_moniker = Some(props.moniker.clone().native_into_fidl());
            request.event_stream_scope = Some(props.scope.clone().native_into_fidl());
        }
    }
}

#[async_trait]
impl<C: CapabilityBound> Routable<C> for IntermediateRouter
where
    Arc<Router<C>>: TryFrom<Capability>,
    Arc<C>: TryFrom<Capability>,
    Router<C>: CapabilityBound,
    C: std::fmt::Debug,
{
    async fn route(
        &self,
        mut request: fruntime::RouteRequest,
        target: Arc<WeakInstanceToken>,
    ) -> Result<Option<Arc<C>>, RouterError> {
        #[cfg(target_os = "fuchsia")]
        let _trace_guard = self.enable_tracing.then(|| {
            trace::async_enter!(
                trace::Id::new(),
                "component_manager",
                "route_capability",
                "target" => self.moniker.as_str(),
                "type" => self.type_name.to_string().as_str(),
                "capability" => self.source_path.iter_segments().join("/").as_str()
            )
        });

        self.handle_new_request(&mut request)?;
        match self.get_source_router(&request, false).await? {
            RouterCapabilityOrSource::Capability(c) => Ok(Some(c)),
            RouterCapabilityOrSource::Source(source) => {
                match &*source {
                    CapabilitySource::Void(_) => (),
                    other_source => {
                        panic!(
                            "should only return source for non-debug routes when source is void, but the source is {other_source:?}"
                        );
                    }
                }
                Err(RoutingError::SourceCapabilityIsVoid { moniker: source.source_moniker() }
                    .into())
            }
            RouterCapabilityOrSource::Router(router) => router.route(request, target).await,
        }
    }

    async fn route_debug(
        &self,
        mut request: fruntime::RouteRequest,
        target: Arc<WeakInstanceToken>,
    ) -> Result<CapabilitySource, RouterError> {
        #[cfg(target_os = "fuchsia")]
        let _trace_guard = self.enable_tracing.then(|| {
            trace::async_enter!(
                trace::Id::new(),
                "component_manager",
                "route_capability_debug",
                "target" => self.moniker.as_str(),
                "type" => self.type_name.to_string().as_str(),
                "capability" => self.source_path.iter_segments().join("/").as_str()
            )
        });

        self.handle_new_request(&mut request)?;
        match self.get_source_router(&request, true).await? {
            RouterCapabilityOrSource::Capability(_) => {
                panic!("returned capability for debug operation")
            }
            RouterCapabilityOrSource::Source(source) => Ok(*source),
            RouterCapabilityOrSource::Router(router) => router.route_debug(request, target).await,
        }
    }

    fn error_info(&self) -> Option<RouterErrorInfo> {
        Some(RouterErrorInfo {
            capability_type: self.type_name,
            name: self.source_path.basename().unwrap().to_owned(),
            availability: self.availability.unwrap(),
        })
    }
}
