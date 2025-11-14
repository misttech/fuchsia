// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::builtin::runner::BuiltinRunner;
use crate::model::component::manager::ComponentManagerInstance;
use crate::model::component::{ComponentInstance, WeakComponentInstance};
use crate::model::resolver::Resolver;
use crate::model::routing::RoutingFailureErrorReporter;
use crate::sandbox_util::{LaunchTaskOnReceive, take_handle_as_stream};
use ::routing::bedrock::dict_ext::DictExt;
use ::routing::bedrock::request_metadata::Metadata;
use ::routing::bedrock::structured_dict::ComponentInput;
use ::routing::bedrock::with_porcelain::WithPorcelain;
use ::routing::capability_source::{
    BuiltinSource, CapabilitySource, ComponentCapability, InternalCapability, NamespaceSource,
};
use ::routing::error::{ErrorReporter, RouteRequestErrorInfo};
use ::routing::policy::{GlobalPolicyChecker, ScopedPolicyChecker};
use ::routing::resolving::ComponentAddress;
use anyhow::format_err;
use async_trait::async_trait;
use cm_config::{RuntimeConfig, SecurityPolicy};
use cm_rust::{Availability, CapabilityTypeName};
use cm_types::{Name, RelativePath, Url};
use fidl::endpoints::{DiscoverableProtocolMarker, ProtocolMarker, ServerEnd};
use futures::future::BoxFuture;
use futures::{FutureExt, TryStreamExt, future};
use hooks::EventType;
use log::warn;
use router_error::RouterError;
use sandbox::{Capability, Data, DirConnector, Router, RouterResponse};
use std::sync::Arc;
use vfs::directory::entry::OpenRequest;
use vfs::{ExecutionScope, Path, ToObjectRequest, WeakExecutionScope};
use {fidl_fuchsia_component_resolution as fresolution, fidl_fuchsia_io as fio};

/// Constructs a [ComponentInput] that contains built-in capabilities.
pub struct RootInputBuilder {
    input: ComponentInput,
    scope: WeakExecutionScope,
    security_policy: Arc<SecurityPolicy>,
    policy_checker: GlobalPolicyChecker,
    builtin_capabilities: Vec<cm_rust::CapabilityDecl>,
    top_instance: Arc<ComponentManagerInstance>,
}

impl RootInputBuilder {
    pub fn new(
        top_instance: &Arc<ComponentManagerInstance>,
        runtime_config: &Arc<RuntimeConfig>,
    ) -> Self {
        Self {
            input: ComponentInput::default(),
            top_instance: top_instance.clone(),
            scope: top_instance.execution_scope().as_weak(),
            security_policy: runtime_config.security_policy.clone(),
            policy_checker: GlobalPolicyChecker::new(runtime_config.security_policy.clone()),
            builtin_capabilities: runtime_config.builtin_capabilities.clone(),
        }
    }

    /// Adds a new builtin protocol to the input that will be given to the root component. If the
    /// protocol is not listed in `self.builtin_capabilities`, then it will silently be omitted
    /// from the input.
    pub fn add_builtin_protocol_if_enabled<P>(
        &mut self,
        task_to_launch: impl Fn(P::RequestStream) -> BoxFuture<'static, Result<(), anyhow::Error>>
        + Sync
        + Send
        + 'static,
    ) where
        P: DiscoverableProtocolMarker + ProtocolMarker,
    {
        let name = Name::new(P::PROTOCOL_NAME).unwrap();
        self.add_named_builtin_protocol_if_enabled::<P>(name, task_to_launch)
    }

    /// Adds a new builtin protocol to the input that will be given to the root component. If the
    /// protocol is not listed in `self.builtin_capabilities`, then it will silently be omitted
    /// from the input.
    /// The protocol's name, which is the value checked for in `self.builtin_capabilities` and how
    /// the protocol is exposed to the root component, will be `name` instead of `P::PROTOCOL_NAME`.
    pub fn add_named_builtin_protocol_if_enabled<P>(
        &mut self,
        name: cm_types::Name,
        task_to_launch: impl Fn(P::RequestStream) -> BoxFuture<'static, Result<(), anyhow::Error>>
        + Sync
        + Send
        + 'static,
    ) where
        P: DiscoverableProtocolMarker + ProtocolMarker,
    {
        // TODO: check capability type too
        // TODO: if we store the capabilities in a hashmap by name, then we can remove them as
        // they're added and confirm at the end that we've not been asked to enable something
        // unknown.
        if self.builtin_capabilities.iter().find(|decl| decl.name() == &name).is_none() {
            // This builtin protocol is not enabled based on the runtime config, so don't add the
            // capability to the input.
            return;
        }

        let capability_source = CapabilitySource::Builtin(BuiltinSource {
            capability: InternalCapability::Protocol(name.clone()),
        });

        let launch = LaunchTaskOnReceive::new(
            capability_source,
            self.scope.clone(),
            name.clone(),
            Some(self.policy_checker.clone()),
            Arc::new(move |server_end, _, _, _| {
                task_to_launch(take_handle_as_stream::<P>(server_end)).boxed()
            }),
        );

        let router = launch.into_router();
        match self.input.insert_capability(
            &name,
            WithPorcelain::<_, _, ComponentInstance>::with_porcelain_no_default(
                router,
                CapabilityTypeName::Protocol,
            )
            .availability(Availability::Required)
            .target_above_root(&self.top_instance)
            .error_info(RouteRequestErrorInfo::for_builtin(CapabilityTypeName::Protocol, &name))
            .error_reporter(NullErrorReporter {})
            .build()
            .into(),
        ) {
            Ok(()) => (),
            Err(e) => warn!("failed to add {name} to root component input: {e:?}"),
        }
    }

    pub fn add_namespace_protocol(&mut self, protocol: &cm_rust::ProtocolDecl) {
        let path = protocol.source_path.as_ref().unwrap().to_string();
        let capability_source = CapabilitySource::Namespace(NamespaceSource {
            capability: ComponentCapability::Protocol(protocol.clone()),
        });
        let launch = LaunchTaskOnReceive::new(
            capability_source,
            self.scope.clone(),
            "namespace capability dispatcher",
            Some(self.policy_checker.clone()),
            Arc::new(move |server_end, _, _, _| {
                let path = path.clone();
                let fut = async move {
                    fuchsia_fs::node::open_channel_in_namespace(
                        &path,
                        fio::Flags::empty(),
                        ServerEnd::new(server_end),
                    )
                    .map_err(|e| {
                        warn!(
                            "failed to open capability in component_manager's namespace \
                    \"{path}\": {e}"
                        );
                        format_err!("{e:?}")
                    })
                };
                fut.boxed()
            }),
        );
        let router = launch.into_router();
        match self.input.insert_capability(
            &protocol.name,
            WithPorcelain::<_, _, ComponentInstance>::with_porcelain_no_default(
                router,
                CapabilityTypeName::Protocol,
            )
            .availability(Availability::Required)
            .target_above_root(&self.top_instance)
            .error_info(RouteRequestErrorInfo::for_builtin(
                CapabilityTypeName::Protocol,
                &protocol.name,
            ))
            .error_reporter(NullErrorReporter {})
            .build()
            .into(),
        ) {
            Ok(()) => (),
            Err(e) => warn!("failed to add {} to root component input: {e:?}", protocol.name),
        }
    }

    pub fn add_namespace_directory(&mut self, directory: &cm_rust::DirectoryDecl) {
        let path = directory.source_path.as_ref().unwrap().clone();
        let capability_source = CapabilitySource::Namespace(NamespaceSource {
            capability: ComponentCapability::Directory(directory.clone()),
        });
        let router =
            Router::<DirConnector>::new(move |request: Option<sandbox::Request>, debug| {
                if debug {
                    return futures::future::ready(Ok(RouterResponse::Debug(
                        capability_source
                            .clone()
                            .try_into()
                            .expect("failed to convert capability source to Data"),
                    )))
                    .boxed();
                }
                let mut path = path.clone();
                async move {
                    let request = request.ok_or(RouterError::InvalidArgs)?;
                    let rights: ::routing::rights::Rights =
                        request.metadata.get_metadata().ok_or(RouterError::InvalidArgs)?;
                    let subdir: ::routing::subdir::SubDir = request
                        .metadata
                        .get_metadata()
                        .or(Some(::routing::subdir::SubDir::dot()))
                        .unwrap();
                    let success = path.extend(subdir.clone().into());
                    if !success {
                        return Err(::routing::error::RoutingError::PathTooLong {
                            moniker: moniker::ExtendedMoniker::ComponentManager,
                            path: format!("{path}/{subdir}"),
                            keyword: "subdir".to_string(),
                        }
                        .into());
                    }
                    let path = path.to_string();
                    let flags = fio::Flags::from_bits(rights.into()).unwrap();
                    let dir_proxy = match fuchsia_fs::directory::open_in_namespace(&path, flags) {
                        Ok(proxy) => proxy,
                        Err(e) => {
                            warn!(
                                "failed to open path {} in component manager's namespace: {:?}",
                                path, e
                            );
                            return Err(RouterError::Internal);
                        }
                    };
                    let dir_connector =
                        DirConnector::from_proxy(dir_proxy, RelativePath::dot(), flags);
                    Ok(RouterResponse::Capability(dir_connector))
                }
                .boxed()
            });
        let router = WithPorcelain::<_, _, ComponentInstance>::with_porcelain_no_default(
            router,
            CapabilityTypeName::Directory,
        )
        .availability(Availability::Required)
        .rights(Some(directory.rights.into()))
        .target_above_root(&self.top_instance)
        .error_info(RouteRequestErrorInfo::from(&cm_rust::CapabilityDecl::Directory(
            directory.clone(),
        )))
        .error_reporter(RoutingFailureErrorReporter::new())
        .build();
        match self.input.insert_capability(&directory.name, router.into()) {
            Ok(()) => (),
            Err(e) => warn!("failed to add {} to root component input: {e:?}", directory.name),
        }
    }

    pub fn add_resolver(
        &mut self,
        resolver_schema: String,
        resolver: Arc<dyn Resolver + Send + Sync + 'static>,
    ) {
        let resolver_schema = Name::new(resolver_schema)
            .expect("invalid resolver schema, this should be prevented by manifest_validation");
        let capability_source = CapabilitySource::Builtin(BuiltinSource {
            capability: InternalCapability::Resolver(resolver_schema.clone()),
        });
        let resolver = Arc::new(resolver);
        async fn do_resolve(
            weak_target: &WeakComponentInstance,
            resolver: &Arc<dyn Resolver + Send + Sync>,
            url: String,
            context: Option<fresolution::Context>,
        ) -> Result<fresolution::Component, fresolution::ResolverError> {
            let target = weak_target.upgrade().map_err(|_| fresolution::ResolverError::Internal)?;
            let url = Url::new(url).map_err(|_| fresolution::ResolverError::InvalidArgs)?;
            let component_address = match context {
                Some(context) => {
                    ComponentAddress::from_url_and_context(&url, context.into(), &target).await
                }
                None => ComponentAddress::from_url(&url, &target).await,
            }
            .map_err(|_| fresolution::ResolverError::InvalidArgs)?;
            let component = resolver.resolve(&component_address).await?;
            Ok(component.into())
        }
        let name_for_warn = resolver_schema.clone();
        let launch = LaunchTaskOnReceive::new(
            capability_source,
            self.scope.clone(),
            resolver_schema.clone(),
            Some(self.policy_checker.clone()),
            Arc::new(move |server_end, weak_target, _, _| {
                let resolver = resolver.clone();
                let name_for_warn = name_for_warn.clone();
                async move {
                    let mut stream =
                        take_handle_as_stream::<fresolution::ResolverMarker>(server_end);
                    while let Some(request) = stream.try_next().await? {
                        match request {
                            fresolution::ResolverRequest::Resolve { component_url, responder } => {
                                responder.send(
                                    do_resolve(&weak_target, &resolver, component_url, None).await,
                                )?;
                            }
                            fresolution::ResolverRequest::ResolveWithContext {
                                component_url,
                                context,
                                responder,
                            } => {
                                responder.send(
                                    do_resolve(
                                        &weak_target,
                                        &resolver,
                                        component_url,
                                        Some(context),
                                    )
                                    .await,
                                )?;
                            }
                            other_request => warn!(
                                "unexpected resolver request received for resolver {}: {:?}",
                                name_for_warn, other_request
                            ),
                        };
                    }
                    Ok(())
                }
                .boxed()
            }),
        );
        // TODO(https://fxbug.dev/369573212): Historically the fuchsia-boot resolver has been
        // placed in the root component's environment as `fuchsia-boot` and offered to the root
        // component as `boot_resolver`. This discrepancy must be handled here, as existing tests
        // and production manifests expect this
        // behavior.
        let resolver_name_str = match resolver_schema.as_str() {
            "fuchsia-boot" => "boot_resolver".to_string(),
            resolver_name => resolver_name.to_string(),
        };
        let resolver_name = Name::new(resolver_name_str)
            .expect("invalid resolver name, this should be prevented by manifest_validation");

        let r = launch.into_router();
        let r = WithPorcelain::<_, _, ComponentInstance>::with_porcelain_no_default(
            r,
            CapabilityTypeName::Resolver,
        )
        .availability(Availability::Required)
        .target_above_root(&self.top_instance)
        .error_info(RouteRequestErrorInfo::for_builtin(
            CapabilityTypeName::Resolver,
            &resolver_name,
        ))
        .error_reporter(NullErrorReporter {})
        .build();
        if let Err(e) =
            self.input.capabilities().insert_capability(&resolver_name, r.clone().into())
        {
            warn!(
                "failed to add resolver {} to root component offered capabilities: {e:?}",
                resolver_name
            );
        }
        if let Err(e) =
            self.input.environment().resolvers().insert_capability(&resolver_schema, r.into())
        {
            warn!("failed to add resolver {} to root component environment: {e:?}", resolver_schema)
        }
    }

    pub fn add_runner_if_enabled(&mut self, runner: BuiltinRunner) {
        if self.builtin_capabilities.iter().find(|decl| decl.name() == runner.name()).is_none() {
            // This builtin protocol is not enabled based on the runtime config, so don't add the
            // capability to the input.
            return;
        }
        self.add_runner(runner);
    }

    pub fn add_runner(&mut self, runner: BuiltinRunner) {
        let name = runner.name().clone();
        let add_to_env = runner.add_to_env();
        let capability_source = CapabilitySource::Builtin(BuiltinSource {
            capability: InternalCapability::Runner(name.clone()),
        });
        let security_policy = self.security_policy.clone();
        let execution_scope = ExecutionScope::new();
        let launch = LaunchTaskOnReceive::new(
            capability_source,
            self.scope.clone(),
            runner.name().clone(),
            Some(self.policy_checker.clone()),
            Arc::new(move |server_end, weak_component, _, _| {
                const FLAGS: fio::Flags = fio::Flags::PROTOCOL_SERVICE;
                let mut object_request = FLAGS.to_object_request(server_end);
                runner
                    .factory()
                    .clone()
                    .get_scoped_runner(
                        ScopedPolicyChecker::new(security_policy.clone(), weak_component.moniker),
                        OpenRequest::new(
                            execution_scope.clone(),
                            FLAGS,
                            Path::dot(),
                            &mut object_request,
                        ),
                    )
                    .expect("TODO");
                future::ready(Ok(())).boxed()
            }),
        );

        let r = launch.into_router();
        let r = WithPorcelain::<_, _, ComponentInstance>::with_porcelain_no_default(
            r,
            CapabilityTypeName::Runner,
        )
        .availability(Availability::Required)
        .target_above_root(&self.top_instance)
        .error_info(RouteRequestErrorInfo::for_builtin(CapabilityTypeName::Runner, &name))
        .error_reporter(NullErrorReporter {})
        .build();
        if let Err(e) = self.input.capabilities().insert_capability(&name, r.clone().into()) {
            warn!("failed to add runner {} to root component offered capabilities: {e:?}", name);
        }
        if add_to_env {
            if let Err(e) = self.input.environment().runners().insert_capability(&name, r.into()) {
                warn!("failed to add runner {} to root component environment: {e:?}", name)
            }
        }
    }

    pub fn add_event_stream_capabilities(&self) {
        for event_type in EventType::values() {
            let router = Router::new(move |request: Option<sandbox::Request>, debug| {
                async move {
                    if debug {
                        let name = Name::new(event_type.as_str()).unwrap();
                        let capability_source = CapabilitySource::Builtin(BuiltinSource {
                            capability: InternalCapability::EventStream(name),
                        });
                        return Ok(RouterResponse::Debug(capability_source.try_into().unwrap()));
                    }
                    let request = request.expect("missing request on event stream route");
                    let request_metadata = request.metadata;
                    let _ = request_metadata.insert(
                        Name::new("event_stream_name").unwrap(),
                        Capability::Data(Data::String(event_type.to_string().into())),
                    );
                    Ok(RouterResponse::Capability(request_metadata))
                }
                .boxed()
            });
            let name = Name::new(event_type.as_str()).unwrap();
            if let Err(e) = self.input.capabilities().insert_capability(&name, router.into()) {
                warn!(
                    "failed to add event_stream {} to root component offered capabilities: {e:?}",
                    name
                );
            }
        }
    }

    pub fn build(self) -> ComponentInput {
        self.input
    }
}

#[derive(Clone)]
struct NullErrorReporter {}
#[async_trait]
impl ErrorReporter for NullErrorReporter {
    async fn report(
        &self,
        _: &RouteRequestErrorInfo,
        _: &RouterError,
        _: sandbox::WeakInstanceToken,
    ) {
    }
}
