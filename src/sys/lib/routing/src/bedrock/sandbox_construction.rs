// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::bedrock::aggregate_router::{AggregateRouterFn, AggregateSource};
use crate::bedrock::structured_dict::{
    ComponentEnvironment, ComponentInput, ComponentOutput, StructuredDictMap,
};
use crate::bedrock::use_dictionary_router::UseDictionaryRouter;
use crate::bedrock::with_service_renames_and_filter::WithServiceRenamesAndFilter;
use crate::component_instance::{ComponentInstanceInterface, WeakComponentInstanceInterface};
use crate::error::{ErrorReporter, RouteRequestErrorInfo, RoutingError};
use crate::{DictExt, LazyGet, WithPorcelain};
use async_trait::async_trait;
use capability_source::{
    AggregateCapability, AggregateInstance, AggregateMember, AnonymizedAggregateSource,
    CapabilitySource, ComponentCapability, ComponentSource, FilteredAggregateProviderSource,
    InternalCapability, InternalEventStreamCapability, VoidSource,
};
use cm_rust::offer::{OfferDecl, OfferDeclCommon};
use cm_rust::{
    CapabilityTypeName, DictionaryValue, ExposeDecl, ExposeDeclCommon, FidlIntoNative, SourceName,
    SourcePath, UseDeclCommon,
};
use cm_types::{Availability, BorrowedSeparatedPath, IterablePath, Name, SeparatedPath};
use fidl::endpoints::DiscoverableProtocolMarker;
use fidl_fuchsia_component as fcomponent;
use fidl_fuchsia_component_decl as fdecl;
use fidl_fuchsia_component_runtime::RouteRequest;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_sys2 as fsys;
use fuchsia_sync::Mutex;
use futures::FutureExt;
use itertools::Itertools;
use log::warn;
use moniker::{ChildName, Moniker};
use router_error::RouterError;
use runtime_capabilities::{
    Capability, CapabilityBound, Connector, Data, Dictionary, DirConnector, Routable, Router,
    WeakInstanceToken,
};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Debug;
use std::sync::{Arc, LazyLock};

/// This type comes from `UseEventStreamDecl`.
pub type EventStreamFilter = Option<BTreeMap<String, DictionaryValue>>;

/// Contains all of the information needed to find and use a source of an event stream.
#[derive(Clone)]
pub struct EventStreamSourceRouter {
    /// The source router that should return a dictionary detailing specifics on the event stream
    /// such as its type and scope.
    pub router: Arc<Router<Dictionary>>,
    /// The filter that should be applied on the event stream initialized from the information
    /// returned by the router.
    pub filter: EventStreamFilter,
}
pub type EventStreamUseRouterFn<C> =
    dyn Fn(&Arc<C>, Vec<EventStreamSourceRouter>) -> Arc<Router<Connector>>;

static NAMESPACE: LazyLock<Name> = LazyLock::new(|| "namespace".parse().unwrap());
static NUMBERED_HANDLES: LazyLock<Name> = LazyLock::new(|| "numbered_handles".parse().unwrap());
static RUNNER: LazyLock<Name> = LazyLock::new(|| "runner".parse().unwrap());
static CONFIG: LazyLock<Name> = LazyLock::new(|| "config".parse().unwrap());

/// All capabilities that are available to a component's program.
#[derive(Debug, Clone)]
pub struct ProgramInput {
    // This will always have the following fields:
    // - namespace: Arc<Dictionary>
    // - runner: Option<Arc<Router<Connector>>>
    // - config: Arc<Dictionary>
    // - numbered_handles: Arc<Dictionary>
    inner: Arc<Dictionary>,
}

impl Default for ProgramInput {
    fn default() -> Self {
        Self::new(Dictionary::new(), None, Dictionary::new())
    }
}

impl From<ProgramInput> for Arc<Dictionary> {
    fn from(program_input: ProgramInput) -> Self {
        program_input.inner
    }
}

impl ProgramInput {
    pub fn new(
        namespace: Arc<Dictionary>,
        runner: Option<Arc<Router<Connector>>>,
        config: Arc<Dictionary>,
    ) -> Self {
        let inner = Dictionary::new();
        inner.insert(NAMESPACE.clone(), Capability::Dictionary(namespace));
        if let Some(runner) = runner {
            inner.insert(RUNNER.clone(), Capability::ConnectorRouter(runner));
        }
        inner.insert(NUMBERED_HANDLES.clone(), Capability::Dictionary(Dictionary::new()));
        inner.insert(CONFIG.clone(), Capability::Dictionary(config));
        ProgramInput { inner }
    }

    /// All of the capabilities that appear in a program's namespace.
    pub fn namespace(&self) -> Arc<Dictionary> {
        let cap = self.inner.get(&*NAMESPACE).unwrap();
        let Capability::Dictionary(dict) = cap else {
            unreachable!("namespace entry must be a dictionary: {cap:?}");
        };
        dict
    }

    /// All of the capabilities that appear in a program's set of numbered handles.
    pub fn numbered_handles(&self) -> Arc<Dictionary> {
        let cap = self.inner.get(&*NUMBERED_HANDLES).unwrap();
        let Capability::Dictionary(dict) = cap else {
            unreachable!("numbered_handles entry must be a dictionary: {cap:?}");
        };
        dict
    }

    /// A router for the runner that a component has used (if any).
    pub fn runner(&self) -> Option<Arc<Router<Connector>>> {
        let cap = self.inner.get(&*RUNNER);
        match cap {
            None => None,
            Some(Capability::ConnectorRouter(r)) => Some(r),
            cap => unreachable!("runner entry must be a router: {cap:?}"),
        }
    }

    fn set_runner(&self, capability: Capability) {
        let _ = self.inner.insert(RUNNER.clone(), capability);
    }

    /// All of the config capabilities that a program will use.
    pub fn config(&self) -> Arc<Dictionary> {
        let cap = self.inner.get(&*CONFIG).unwrap();
        let Capability::Dictionary(dict) = cap else {
            unreachable!("config entry must be a dictionary: {cap:?}");
        };
        dict
    }
}

/// A component's sandbox holds all the routing dictionaries that a component has once its been
/// resolved.
#[derive(Debug)]
pub struct ComponentSandbox {
    /// The dictionary containing all capabilities that a component's parent provided to it.
    pub component_input: ComponentInput,

    /// The dictionary containing all capabilities that a component makes available to its parent.
    pub component_output: ComponentOutput,

    /// The dictionary containing all capabilities that are available to a component's program.
    pub program_input: ProgramInput,

    /// The dictionary containing all capabilities that a component's program can provide.
    pub program_output_dict: Arc<Dictionary>,

    /// Router that returns the dictionary of framework capabilities scoped to a component. This a
    /// Router rather than the Dictionary itself to save memory.
    ///
    /// REQUIRES: This Router must never poll. This constraint exists `build_component_sandbox` is
    /// not async.
    // NOTE: This is wrapped in Mutex for interior mutability so that it is modifiable like the
    // other parts of the sandbox. If this were a Dictionary this wouldn't be necessary because
    // Dictionary already supports interior mutability, but since this is a singleton we don't need
    // a Dictionary here. The Arc around the Mutex is needed for Sync.
    framework_router: Mutex<Arc<Router<Dictionary>>>,

    /// The dictionary containing all capabilities that a component declares based on another
    /// capability. Currently this is only the storage admin protocol.
    pub capability_sourced_capabilities_dict: Arc<Dictionary>,

    /// The dictionary containing all dictionaries declared by this component.
    pub declared_dictionaries: Arc<Dictionary>,

    /// This set holds a component input dictionary for each child of a component. Each dictionary
    /// contains all capabilities the component has made available to a specific collection.
    pub child_inputs: StructuredDictMap<ComponentInput>,

    /// This set holds a component input dictionary for each collection declared by a component.
    /// Each dictionary contains all capabilities the component has made available to a specific
    /// collection.
    pub collection_inputs: StructuredDictMap<ComponentInput>,
}

impl Default for ComponentSandbox {
    fn default() -> Self {
        static NULL_ROUTER: LazyLock<Arc<Router<Dictionary>>> =
            LazyLock::new(|| Router::new(NullRouter {}));
        struct NullRouter;
        #[async_trait]
        impl Routable<Dictionary> for NullRouter {
            async fn route(
                &self,
                _request: RouteRequest,
                _target: Arc<WeakInstanceToken>,
            ) -> Result<Option<Arc<Dictionary>>, RouterError> {
                panic!("null router invoked");
            }
            async fn route_debug(
                &self,
                _request: RouteRequest,
                _target: Arc<WeakInstanceToken>,
            ) -> Result<CapabilitySource, RouterError> {
                panic!("null router invoked");
            }
        }
        let framework_router = Mutex::new(NULL_ROUTER.clone());
        Self {
            framework_router,
            component_input: Default::default(),
            component_output: Default::default(),
            program_input: Default::default(),
            program_output_dict: Default::default(),
            capability_sourced_capabilities_dict: Default::default(),
            declared_dictionaries: Default::default(),
            child_inputs: Default::default(),
            collection_inputs: Default::default(),
        }
    }
}

impl From<ComponentSandbox> for Arc<Dictionary> {
    fn from(sandbox: ComponentSandbox) -> Arc<Dictionary> {
        let sandbox_dictionary = Dictionary::new();
        sandbox_dictionary.insert(
            Name::new("framework").unwrap(),
            Capability::DictionaryRouter(sandbox.framework_router.lock().clone()),
        );
        sandbox_dictionary.insert(
            Name::new("component_input").unwrap(),
            Capability::Dictionary(sandbox.component_input.into()),
        );
        sandbox_dictionary.insert(
            Name::new("component_output").unwrap(),
            Capability::Dictionary(sandbox.component_output.into()),
        );
        sandbox_dictionary.insert(
            Name::new("program_input").unwrap(),
            Capability::Dictionary(sandbox.program_input.into()),
        );
        sandbox_dictionary.insert(
            Name::new("program_output").unwrap(),
            Capability::Dictionary(sandbox.program_output_dict),
        );
        sandbox_dictionary.insert(
            Name::new("capability_sourced").unwrap(),
            Capability::Dictionary(sandbox.capability_sourced_capabilities_dict),
        );
        sandbox_dictionary.insert(
            Name::new("declared_dictionaries").unwrap(),
            Capability::Dictionary(sandbox.declared_dictionaries),
        );
        sandbox_dictionary.insert(
            Name::new("child_inputs").unwrap(),
            Capability::Dictionary(sandbox.child_inputs.into()),
        );
        sandbox_dictionary.insert(
            Name::new("collection_inputs").unwrap(),
            Capability::Dictionary(sandbox.collection_inputs.into()),
        );
        sandbox_dictionary
    }
}

impl Clone for ComponentSandbox {
    fn clone(&self) -> Self {
        let Self {
            component_input,
            component_output,
            program_input,
            program_output_dict,
            framework_router,
            capability_sourced_capabilities_dict,
            declared_dictionaries,
            child_inputs,
            collection_inputs,
        } = self;
        Self {
            component_input: component_input.clone(),
            component_output: component_output.clone(),
            program_input: program_input.clone(),
            program_output_dict: program_output_dict.clone(),
            framework_router: Mutex::new(framework_router.lock().clone()),
            capability_sourced_capabilities_dict: capability_sourced_capabilities_dict.clone(),
            declared_dictionaries: declared_dictionaries.clone(),
            child_inputs: child_inputs.clone(),
            collection_inputs: collection_inputs.clone(),
        }
    }
}

impl ComponentSandbox {
    /// Copies all of the entries from the given sandbox into this one. Panics if the given sandbox
    /// is holding any entries that cannot be copied. Panics if there are any duplicate entries.
    pub fn append(&self, sandbox: &ComponentSandbox) {
        // We destructure the sandbox here to ensure that this code is updated if the contents of
        // the sandbox change.
        let ComponentSandbox {
            component_input,
            component_output,
            program_input,
            program_output_dict,
            framework_router,
            capability_sourced_capabilities_dict,
            declared_dictionaries,
            child_inputs,
            collection_inputs,
        } = sandbox;
        for (copy_from, copy_to) in [
            (&component_input.capabilities(), &self.component_input.capabilities()),
            (&component_input.environment().debug(), &self.component_input.environment().debug()),
            (
                &component_input.environment().runners(),
                &self.component_input.environment().runners(),
            ),
            (
                &component_input.environment().resolvers(),
                &self.component_input.environment().resolvers(),
            ),
            (&component_output.capabilities(), &self.component_output.capabilities()),
            (&component_output.framework(), &self.component_output.framework()),
            (&program_input.namespace(), &self.program_input.namespace()),
            (&program_input.numbered_handles(), &self.program_input.numbered_handles()),
            (&program_input.config(), &self.program_input.config()),
            (&program_output_dict, &self.program_output_dict),
            (&capability_sourced_capabilities_dict, &self.capability_sourced_capabilities_dict),
            (&declared_dictionaries, &self.declared_dictionaries),
        ] {
            copy_to.append(copy_from).expect("conflicting entry found");
        }
        if let Some(timeout) = component_input.environment().stop_timeout() {
            self.component_input.environment().set_stop_timeout(timeout as i64);
        }
        *self.framework_router.lock() = framework_router.lock().clone();
        if let Some(runner_router) = program_input.runner() {
            self.program_input.set_runner(runner_router.into());
        }
        self.child_inputs.append(child_inputs).unwrap();
        self.collection_inputs.append(collection_inputs).unwrap();
    }

    pub fn framework_router(&self) -> Arc<Router<Dictionary>> {
        self.framework_router.lock().clone()
    }
}

/// Once a component has been resolved and its manifest becomes known, this function produces the
/// various dicts the component needs based on the contents of its manifest.
pub fn build_component_sandbox<C: ComponentInstanceInterface + 'static>(
    component: &Arc<C>,
    child_component_output_dictionary_routers: HashMap<ChildName, Arc<Router<Dictionary>>>,
    decl: &cm_rust::ComponentDecl,
    component_input: ComponentInput,
    program_output_dict: Arc<Dictionary>,
    framework_router: Arc<Router<Dictionary>>,
    capability_sourced_capabilities_dict: Arc<Dictionary>,
    declared_dictionaries: Arc<Dictionary>,
    error_reporter: impl ErrorReporter,
    aggregate_router_fn: &AggregateRouterFn<C>,
    event_stream_use_router_fn: &EventStreamUseRouterFn<C>,
) -> ComponentSandbox {
    let component_output = ComponentOutput::new();
    let program_input = ProgramInput::default();
    let environments: StructuredDictMap<ComponentEnvironment> = Default::default();
    let child_inputs: StructuredDictMap<ComponentInput> = Default::default();
    let collection_inputs: StructuredDictMap<ComponentInput> = Default::default();

    for environment_decl in &decl.environments {
        let _ = environments.insert(
            environment_decl.name.clone(),
            build_environment(
                component,
                &child_component_output_dictionary_routers,
                &component_input,
                environment_decl,
                &program_output_dict,
                &error_reporter,
            ),
        );
    }

    for child in &decl.children {
        let environment;
        if let Some(environment_name) = child.environment.as_ref() {
            environment = environments.get(environment_name).expect(
                "child references nonexistent environment, \
                    this should be prevented in manifest validation",
            );
        } else {
            environment = component_input.environment();
        }
        let input = ComponentInput::new(environment);
        let name = Name::new(child.name.as_str()).expect("child is static so name is not long");
        let _ = child_inputs.insert(name, input);
    }

    for collection in &decl.collections {
        let environment;
        if let Some(environment_name) = collection.environment.as_ref() {
            environment = environments.get(environment_name).expect(
                "collection references nonexistent environment, \
                    this should be prevented in manifest validation",
            )
        } else {
            environment = component_input.environment();
        }
        let input = ComponentInput::new(environment);
        let _ = collection_inputs.insert(collection.name.clone(), input);
    }

    let mut dictionary_use_bundles = Vec::with_capacity(decl.uses.len());
    for use_bundle in group_use_aggregates(&decl.uses).into_iter() {
        let first_use = *use_bundle.first().unwrap();
        match first_use {
            cm_rust::UseDecl::Service(_)
                if matches!(first_use.source(), cm_rust::UseSource::Collection(_)) =>
            {
                let cm_rust::UseSource::Collection(collection_name) = first_use.source() else {
                    unreachable!();
                };
                let availability = *first_use.availability();
                let aggregate = (aggregate_router_fn)(
                    component.clone(),
                    vec![AggregateSource::Collection { collection_name: collection_name.clone() }],
                    CapabilitySource::AnonymizedAggregate(AnonymizedAggregateSource {
                        capability: AggregateCapability::Service(first_use.source_name().clone()),
                        moniker: component.moniker().clone(),
                        members: vec![AggregateMember::try_from(first_use).unwrap()],
                        instances: vec![],
                    }),
                )
                .with_porcelain_with_default(CapabilityTypeName::Service)
                .availability(availability)
                .target(component)
                .error_info(first_use)
                .error_reporter(error_reporter.clone())
                .log_errors()
                .build();
                let prev = program_input
                    .namespace()
                    .insert_capability(first_use.path().unwrap(), aggregate.into());
                assert!(
                    prev.is_none(),
                    "failed to insert {}: preexisting value",
                    first_use.path().unwrap()
                );
            }
            cm_rust::UseDecl::Service(_) => extend_dict_with_use::<DirConnector, _>(
                component,
                &child_component_output_dictionary_routers,
                &component_input,
                &program_input,
                &program_output_dict,
                &framework_router,
                &capability_sourced_capabilities_dict,
                first_use,
                error_reporter.clone(),
            ),
            cm_rust::UseDecl::Directory(_) | cm_rust::UseDecl::Storage(_) => {
                extend_dict_with_use::<DirConnector, C>(
                    component,
                    &child_component_output_dictionary_routers,
                    &component_input,
                    &program_input,
                    &program_output_dict,
                    &framework_router,
                    &capability_sourced_capabilities_dict,
                    first_use,
                    error_reporter.clone(),
                )
            }
            cm_rust::UseDecl::Protocol(_) | cm_rust::UseDecl::Runner(_) => {
                extend_dict_with_use::<Connector, _>(
                    component,
                    &child_component_output_dictionary_routers,
                    &component_input,
                    &program_input,
                    &program_output_dict,
                    &framework_router,
                    &capability_sourced_capabilities_dict,
                    first_use,
                    error_reporter.clone(),
                )
            }
            cm_rust::UseDecl::Config(config) => extend_dict_with_config_use(
                component,
                &child_component_output_dictionary_routers,
                &component_input,
                &program_input,
                &program_output_dict,
                config,
                error_reporter.clone(),
            ),
            cm_rust::UseDecl::EventStream(_) => extend_dict_with_event_stream_uses(
                component,
                &component_input,
                &program_input,
                use_bundle,
                error_reporter.clone(),
                event_stream_use_router_fn,
            ),
            cm_rust::UseDecl::Dictionary(_) => {
                dictionary_use_bundles.push(use_bundle);
            }
        }
    }

    // The runner may be specified by either use declaration or in the program section of the
    // manifest. If there's no use declaration for a runner and there is one set in the program
    // section, then let's synthesize a use decl for it and add it to the sandbox.
    if !decl.uses.iter().any(|u| matches!(u, cm_rust::UseDecl::Runner(_))) {
        if let Some(runner_name) = decl.program.as_ref().and_then(|p| p.runner.as_ref()) {
            extend_dict_with_use::<Connector, _>(
                component,
                &child_component_output_dictionary_routers,
                &component_input,
                &program_input,
                &program_output_dict,
                &framework_router,
                &capability_sourced_capabilities_dict,
                &cm_rust::UseDecl::Runner(cm_rust::UseRunnerDecl {
                    source: cm_rust::UseSource::Environment,
                    source_name: runner_name.clone(),
                    source_dictionary: Default::default(),
                }),
                error_reporter.clone(),
            )
        }
    }

    // Dictionary uses are special: if any capabilities are used at a path that's a prefix of a
    // dictionary use, then those capabilities are transparently added to the dictionary we
    // assemble in the program input dictionary. In order to do this correctly, we want the program
    // input dictionary to be complete (aside from used dictionaries) so that the dictionaries
    // we're merging with the used dictionaries aren't missing entries. For this reason, we wait
    // until after all other uses are processed before processing used dictionaries.
    for dictionary_use_bundle in dictionary_use_bundles {
        extend_dict_with_dictionary_use(
            component,
            &child_component_output_dictionary_routers,
            &component_input,
            &program_input,
            &program_output_dict,
            &framework_router,
            &capability_sourced_capabilities_dict,
            dictionary_use_bundle,
            error_reporter.clone(),
        )
    }

    for offer_bundle in group_offer_aggregates(&decl.offers) {
        let first_offer = offer_bundle.first().unwrap();
        let get_target_dict = || match first_offer.target() {
            cm_rust::offer::OfferTarget::Child(child_ref) => {
                assert!(child_ref.collection.is_none(), "unexpected dynamic offer target");
                let child_name = Name::new(child_ref.name.as_str())
                    .expect("child is static so name is not long");
                if child_inputs.get(&child_name).is_none() {
                    let _ = child_inputs.insert(child_name.clone(), Default::default());
                }
                child_inputs
                    .get(&child_name)
                    .expect("component input was just added")
                    .capabilities()
            }
            cm_rust::offer::OfferTarget::Collection(name) => {
                if collection_inputs.get(&name).is_none() {
                    let _ = collection_inputs.insert(name.clone(), Default::default());
                }
                collection_inputs
                    .get(&name)
                    .expect("collection input was just added")
                    .capabilities()
            }
            cm_rust::offer::OfferTarget::Capability(name) => {
                let dict = match declared_dictionaries.get(name) {
                    Some(dict) => dict,
                    None => {
                        let dict = Dictionary::new();
                        let _ = declared_dictionaries
                            .insert(name.clone(), Capability::Dictionary(dict.clone()));
                        Capability::Dictionary(dict)
                    }
                };
                let Capability::Dictionary(dict) = dict else {
                    panic!("wrong type in dictionary");
                };
                dict
            }
        };
        match first_offer {
            cm_rust::offer::OfferDecl::Service(_)
                if offer_bundle.len() == 1
                    && !matches!(
                        first_offer.source(),
                        cm_rust::offer::OfferSource::Collection(_)
                    ) =>
            {
                extend_dict_with_offer::<DirConnector, _>(
                    component,
                    &child_component_output_dictionary_routers,
                    &component_input,
                    &program_output_dict,
                    &framework_router,
                    &capability_sourced_capabilities_dict,
                    first_offer,
                    &(get_target_dict)(),
                    error_reporter.clone(),
                )
            }
            cm_rust::offer::OfferDecl::Service(_) => {
                let aggregate_router = new_aggregate_router_from_service_offers(
                    &offer_bundle,
                    component,
                    &child_component_output_dictionary_routers,
                    &component_input,
                    &program_output_dict,
                    &framework_router,
                    &capability_sourced_capabilities_dict,
                    error_reporter.clone(),
                    aggregate_router_fn,
                );
                (get_target_dict)()
                    .insert(first_offer.target_name().clone(), aggregate_router.into());
            }
            cm_rust::offer::OfferDecl::Config(_) => extend_dict_with_offer::<Data, _>(
                component,
                &child_component_output_dictionary_routers,
                &component_input,
                &program_output_dict,
                &framework_router,
                &capability_sourced_capabilities_dict,
                first_offer,
                &(get_target_dict)(),
                error_reporter.clone(),
            ),
            cm_rust::offer::OfferDecl::Directory(_) | cm_rust::offer::OfferDecl::Storage(_) => {
                extend_dict_with_offer::<DirConnector, _>(
                    component,
                    &child_component_output_dictionary_routers,
                    &component_input,
                    &program_output_dict,
                    &framework_router,
                    &capability_sourced_capabilities_dict,
                    first_offer,
                    &(get_target_dict)(),
                    error_reporter.clone(),
                )
            }
            cm_rust::offer::OfferDecl::Dictionary(_)
            | cm_rust::offer::OfferDecl::EventStream(_) => extend_dict_with_offer::<Dictionary, _>(
                component,
                &child_component_output_dictionary_routers,
                &component_input,
                &program_output_dict,
                &framework_router,
                &capability_sourced_capabilities_dict,
                first_offer,
                &(get_target_dict)(),
                error_reporter.clone(),
            ),
            cm_rust::offer::OfferDecl::Protocol(_)
            | cm_rust::offer::OfferDecl::Runner(_)
            | cm_rust::offer::OfferDecl::Resolver(_) => extend_dict_with_offer::<Connector, _>(
                component,
                &child_component_output_dictionary_routers,
                &component_input,
                &program_output_dict,
                &framework_router,
                &capability_sourced_capabilities_dict,
                first_offer,
                &(get_target_dict)(),
                error_reporter.clone(),
            ),
        }
    }

    for expose_bundle in group_expose_aggregates(&decl.exposes) {
        let first_expose = expose_bundle.first().unwrap();
        match first_expose {
            cm_rust::ExposeDecl::Service(_)
                if expose_bundle.len() == 1
                    && !matches!(first_expose.source(), cm_rust::ExposeSource::Collection(_)) =>
            {
                extend_dict_with_expose::<DirConnector, _>(
                    component,
                    &child_component_output_dictionary_routers,
                    &program_output_dict,
                    &framework_router,
                    &capability_sourced_capabilities_dict,
                    first_expose,
                    &component_output,
                    error_reporter.clone(),
                )
            }
            cm_rust::ExposeDecl::Service(_) => {
                let mut aggregate_sources = vec![];
                let temp_component_output = ComponentOutput::new();
                for expose in expose_bundle.iter() {
                    extend_dict_with_expose::<DirConnector, _>(
                        component,
                        &child_component_output_dictionary_routers,
                        &program_output_dict,
                        &framework_router,
                        &capability_sourced_capabilities_dict,
                        expose,
                        &temp_component_output,
                        error_reporter.clone(),
                    );
                    match temp_component_output.capabilities().remove(first_expose.target_name()) {
                        Some(Capability::DirConnectorRouter(router)) => {
                            let source_instance = match expose.source() {
                                cm_rust::ExposeSource::Self_ => AggregateInstance::Self_,
                                cm_rust::ExposeSource::Child(name) => AggregateInstance::Child(
                                    moniker::ChildName::new(name.clone().to_long(), None),
                                ),
                                other_source => {
                                    warn!(
                                        "unsupported source found in expose aggregate: {:?}",
                                        other_source
                                    );
                                    continue;
                                }
                            };
                            aggregate_sources
                                .push(AggregateSource::DirectoryRouter { source_instance, router })
                        }
                        None => match expose.source() {
                            cm_rust::ExposeSource::Collection(collection_name) => {
                                aggregate_sources.push(AggregateSource::Collection {
                                    collection_name: collection_name.clone(),
                                });
                            }
                            _ => continue,
                        },
                        other_value => panic!("unexpected dictionary entry: {:?}", other_value),
                    }
                }
                let availability = *first_expose.availability();
                let aggregate = (aggregate_router_fn)(
                    component.clone(),
                    aggregate_sources,
                    CapabilitySource::AnonymizedAggregate(AnonymizedAggregateSource {
                        capability: AggregateCapability::Service(
                            first_expose.target_name().clone(),
                        ),
                        moniker: component.moniker().clone(),
                        members: expose_bundle
                            .iter()
                            .filter_map(|e| AggregateMember::try_from(*e).ok())
                            .collect(),
                        instances: vec![],
                    }),
                );
                let router = aggregate
                    .with_porcelain_with_default(CapabilityTypeName::Service)
                    .availability(availability)
                    .target(component)
                    .error_info(*first_expose)
                    .error_reporter(error_reporter.clone())
                    .build();
                let target_name = first_expose.target_name().clone();
                let prev = component_output.capabilities().insert(target_name, router.into());
                assert!(
                    prev.is_none(),
                    "failed to insert {}: preexisting value",
                    first_expose.target_name()
                );
            }
            cm_rust::ExposeDecl::Config(_) => extend_dict_with_expose::<Data, _>(
                component,
                &child_component_output_dictionary_routers,
                &program_output_dict,
                &framework_router,
                &capability_sourced_capabilities_dict,
                first_expose,
                &component_output,
                error_reporter.clone(),
            ),
            cm_rust::ExposeDecl::Dictionary(_) => extend_dict_with_expose::<Dictionary, _>(
                component,
                &child_component_output_dictionary_routers,
                &program_output_dict,
                &framework_router,
                &capability_sourced_capabilities_dict,
                first_expose,
                &component_output,
                error_reporter.clone(),
            ),
            cm_rust::ExposeDecl::Directory(_) => extend_dict_with_expose::<DirConnector, _>(
                component,
                &child_component_output_dictionary_routers,
                &program_output_dict,
                &framework_router,
                &capability_sourced_capabilities_dict,
                first_expose,
                &component_output,
                error_reporter.clone(),
            ),
            cm_rust::ExposeDecl::Protocol(_)
            | cm_rust::ExposeDecl::Runner(_)
            | cm_rust::ExposeDecl::Resolver(_) => extend_dict_with_expose::<Connector, _>(
                component,
                &child_component_output_dictionary_routers,
                &program_output_dict,
                &framework_router,
                &capability_sourced_capabilities_dict,
                first_expose,
                &component_output,
                error_reporter.clone(),
            ),
        }
    }

    ComponentSandbox {
        component_input,
        component_output,
        program_input,
        program_output_dict,
        framework_router: Mutex::new(framework_router),
        capability_sourced_capabilities_dict,
        declared_dictionaries,
        child_inputs,
        collection_inputs,
    }
}

fn new_aggregate_router_from_service_offers<C: ComponentInstanceInterface + 'static>(
    offer_bundle: &Vec<&cm_rust::offer::OfferDecl>,
    component: &Arc<C>,
    child_component_output_dictionary_routers: &HashMap<ChildName, Arc<Router<Dictionary>>>,
    component_input: &ComponentInput,
    program_output_dict: &Arc<Dictionary>,
    framework_router: &Arc<Router<Dictionary>>,
    capability_sourced_capabilities_dict: &Arc<Dictionary>,
    error_reporter: impl ErrorReporter,
    aggregate_router_fn: &AggregateRouterFn<C>,
) -> Arc<Router<DirConnector>> {
    let mut aggregate_sources = vec![];
    let dict_for_source_router = Dictionary::new();
    let source = new_aggregate_capability_source(component.moniker().clone(), offer_bundle.clone());
    for offer in offer_bundle.iter() {
        if matches!(&source, &CapabilitySource::FilteredAggregateProvider(_)) {
            if let cm_rust::offer::OfferDecl::Service(offer_service_decl) = offer {
                if offer_service_decl
                    .source_instance_filter
                    .as_ref()
                    .and_then(|v| v.first())
                    .is_none()
                    && offer_service_decl
                        .renamed_instances
                        .as_ref()
                        .and_then(|v| v.first())
                        .is_none()
                {
                    // If we're a filtering aggregate and no filter or renames have been
                    // set, then all instances here are ignored, and there's no point in
                    // including the router in the aggregate.
                    continue;
                }
            }
        }
        extend_dict_with_offer::<DirConnector, _>(
            component,
            &child_component_output_dictionary_routers,
            &component_input,
            &program_output_dict,
            framework_router,
            &capability_sourced_capabilities_dict,
            offer,
            &dict_for_source_router,
            error_reporter.clone(),
        );
        match dict_for_source_router.remove(offer.target_name()) {
            Some(Capability::DirConnectorRouter(router)) => {
                let source_instance = match offer.source() {
                    cm_rust::offer::OfferSource::Self_ => AggregateInstance::Self_,
                    cm_rust::offer::OfferSource::Parent => AggregateInstance::Parent,
                    cm_rust::offer::OfferSource::Child(child_ref) => {
                        AggregateInstance::Child(moniker::ChildName::new(
                            child_ref.name.clone(),
                            child_ref.collection.clone(),
                        ))
                    }
                    other_source => {
                        warn!("unsupported source found in offer aggregate: {:?}", other_source);
                        continue;
                    }
                };
                aggregate_sources.push(AggregateSource::DirectoryRouter { source_instance, router })
            }
            None => match offer.source() {
                // `extend_dict_with_offer` doesn't insert a capability for offers with a source of
                // `OfferSource::Collection`. This is because at this stage there's nothing in the
                // collection, and thus no routers to things in the collection.
                cm_rust::offer::OfferSource::Collection(collection_name) => {
                    aggregate_sources.push(AggregateSource::Collection {
                        collection_name: collection_name.clone(),
                    });
                }
                _ => continue,
            },
            other => warn!("found unexpected entry in dictionary: {:?}", other),
        }
    }
    (aggregate_router_fn)(component.clone(), aggregate_sources, source)
}

fn new_aggregate_capability_source(
    moniker: Moniker,
    offers: Vec<&cm_rust::offer::OfferDecl>,
) -> CapabilitySource {
    let offer_service_decls = offers
        .iter()
        .map(|o| match o {
            cm_rust::offer::OfferDecl::Service(o) => o.as_ref(),
            _ => panic!(
                "cannot aggregate non-service capabilities, manifest validation should prevent this"
            ),
        })
        .collect::<Vec<_>>();
    // This is a filtered offer if any of the offers set a filter or rename mapping.
    let is_filtered_offer = offer_service_decls.iter().any(|o| {
        o.source_instance_filter.as_ref().map(|v| !v.is_empty()).unwrap_or(false)
            || o.renamed_instances.as_ref().map(|v| !v.is_empty()).unwrap_or(false)
    });
    let capability =
        AggregateCapability::Service(offer_service_decls.first().unwrap().target_name.clone());
    if is_filtered_offer {
        CapabilitySource::FilteredAggregateProvider(FilteredAggregateProviderSource {
            capability,
            moniker,
            offer_service_decls: offer_service_decls.into_iter().cloned().collect(),
        })
    } else {
        let members = offers.iter().filter_map(|o| AggregateMember::try_from(*o).ok()).collect();
        CapabilitySource::AnonymizedAggregate(AnonymizedAggregateSource {
            capability,
            moniker,
            members,
            instances: vec![],
        })
    }
}

/// Groups together a set of offers into sub-sets of those that have the same target and target
/// name. This is useful for identifying which offers are part of an aggregation of capabilities,
/// and which are for standalone routes.
fn group_use_aggregates<'a>(
    uses: &'a [cm_rust::UseDecl],
) -> impl Iterator<Item = Vec<&'a cm_rust::UseDecl>> + 'a {
    let mut groupings = HashMap::with_capacity(uses.len());
    let mut ungroupable_uses = Vec::new();
    for use_ in uses.iter() {
        if let Some(target_path) = use_.path() {
            groupings.entry(target_path).or_insert_with(|| Vec::with_capacity(1)).push(use_);
        } else {
            ungroupable_uses.push(use_);
        }
    }
    groupings.into_values().chain(ungroupable_uses.into_iter().map(|u| vec![u]))
}

/// Groups together a set of offers into sub-sets of those that have the same target and target
/// name. This is useful for identifying which offers are part of an aggregation of capabilities,
/// and which are for standalone routes.
fn group_offer_aggregates<'a>(
    offers: &'a [cm_rust::offer::OfferDecl],
) -> impl Iterator<Item = Vec<&'a cm_rust::offer::OfferDecl>> + 'a {
    let mut groupings = HashMap::with_capacity(offers.len());

    for offer in offers {
        groupings
            .entry((offer.target(), offer.target_name()))
            .or_insert_with(|| Vec::with_capacity(1))
            .push(offer);
    }
    groupings.into_values()
}

/// Identical to `group_offer_aggregates`, but for exposes.
fn group_expose_aggregates<'a>(
    exposes: &'a [cm_rust::ExposeDecl],
) -> impl Iterator<Item = Vec<&'a cm_rust::ExposeDecl>> + 'a {
    let mut groupings = HashMap::with_capacity(exposes.len());
    for expose in exposes {
        groupings
            .entry((expose.target(), expose.target_name()))
            .or_insert_with(|| Vec::with_capacity(1))
            .push(expose);
    }
    groupings.into_values()
}

fn build_environment<C: ComponentInstanceInterface + 'static>(
    component: &Arc<C>,
    child_component_output_dictionary_routers: &HashMap<ChildName, Arc<Router<Dictionary>>>,
    component_input: &ComponentInput,
    environment_decl: &cm_rust::EnvironmentDecl,
    program_output_dict: &Arc<Dictionary>,
    error_reporter: &impl ErrorReporter,
) -> ComponentEnvironment {
    let mut environment = ComponentEnvironment::new();
    if environment_decl.extends == fdecl::EnvironmentExtends::Realm {
        environment = component_input.environment().shallow_copy();
    }
    environment.set_name(&environment_decl.name);
    if let Some(stop_timeout_ms) = environment_decl.stop_timeout_ms {
        environment.set_stop_timeout(stop_timeout_ms as i64);
    }
    let debug = environment_decl.debug_capabilities.iter().map(|debug_registration| {
        let cm_rust::DebugRegistration::Protocol(debug) = debug_registration;
        (
            &debug.source_name,
            debug.target_name.clone(),
            &debug.source,
            CapabilityTypeName::Protocol,
            RouteRequestErrorInfo::from(debug_registration),
        )
    });
    let runners = environment_decl.runners.iter().map(|runner| {
        (
            &runner.source_name,
            runner.target_name.clone(),
            &runner.source,
            CapabilityTypeName::Runner,
            RouteRequestErrorInfo::from(runner),
        )
    });
    let resolvers = environment_decl.resolvers.iter().map(|resolver| {
        (
            &resolver.resolver,
            Name::new(&resolver.scheme).unwrap(),
            &resolver.source,
            CapabilityTypeName::Resolver,
            RouteRequestErrorInfo::from(resolver),
        )
    });
    let moniker = component.moniker();
    for (source_name, target_name, source, porcelain_type, route_request) in
        debug.chain(runners).chain(resolvers)
    {
        let source_path =
            SeparatedPath { dirname: Default::default(), basename: source_name.clone() };
        let router: Arc<Router<Connector>> = match &source {
            cm_rust::RegistrationSource::Parent => {
                use_from_parent_router::<Connector>(component_input, source_path, moniker)
            }
            cm_rust::RegistrationSource::Self_ => program_output_dict
                .get_router_or_not_found::<Connector>(
                    &source_path,
                    RoutingError::use_from_self_not_found(
                        moniker,
                        source_path.iter_segments().join("/"),
                    ),
                ),
            cm_rust::RegistrationSource::Child(child_name) => {
                let child_name = ChildName::parse(child_name).expect("invalid child name");
                let Some(child_component_output) =
                    child_component_output_dictionary_routers.get(&child_name)
                else {
                    continue;
                };
                child_component_output.clone().lazy_get(
                    source_path,
                    RoutingError::use_from_child_expose_not_found(
                        &child_name,
                        moniker,
                        source_name.clone(),
                    ),
                )
            }
        };
        let router = router
            .with_porcelain_no_default(porcelain_type)
            .availability(Availability::Required)
            .target(component)
            .error_info(route_request)
            .error_reporter(error_reporter.clone())
            .build();
        let dict_to_insert_to = match porcelain_type {
            CapabilityTypeName::Protocol => environment.debug(),
            CapabilityTypeName::Runner => environment.runners(),
            CapabilityTypeName::Resolver => environment.resolvers(),
            c => panic!("unexpected capability type {}", c),
        };
        let prev =
            dict_to_insert_to.insert_capability(&target_name, Capability::ConnectorRouter(router));
        if prev.is_some() {
            log::warn!("failed to insert {porcelain_type} {target_name}: preexisting value");
        }
    }
    environment
}

/// Extends the given `target_input` to contain the capabilities described in `dynamic_offers`.
pub fn extend_dict_with_offers<C: ComponentInstanceInterface + 'static>(
    component: &Arc<C>,
    static_offers: &[cm_rust::offer::OfferDecl],
    child_component_output_dictionary_routers: &HashMap<ChildName, Arc<Router<Dictionary>>>,
    component_input: &ComponentInput,
    dynamic_offers: &[cm_rust::offer::OfferDecl],
    program_output_dict: &Arc<Dictionary>,
    framework_router: &Arc<Router<Dictionary>>,
    capability_sourced_capabilities_dict: &Arc<Dictionary>,
    target_input: &ComponentInput,
    error_reporter: impl ErrorReporter,
    aggregate_router_fn: &AggregateRouterFn<C>,
) {
    for offer_bundle in group_offer_aggregates(dynamic_offers).into_iter() {
        let first_offer = offer_bundle.first().unwrap();
        match first_offer {
            cm_rust::offer::OfferDecl::Service(_) => {
                let static_offer_bundles = group_offer_aggregates(static_offers);
                let maybe_static_offer_bundle = static_offer_bundles.into_iter().find(|bundle| {
                    bundle.first().unwrap().target_name() == first_offer.target_name()
                });
                let mut combined_offer_bundle = offer_bundle.clone();
                if let Some(mut static_offer_bundle) = maybe_static_offer_bundle {
                    // We are aggregating together dynamic and static offers, as there are static
                    // offers with the same target name as our current dynamic offers. We already
                    // populated a router for the static bundle in the target input, let's toss
                    // that and generate a new one with the expanded set of offers.
                    let _ = target_input.capabilities().remove(first_offer.target_name());
                    combined_offer_bundle.append(&mut static_offer_bundle);
                }
                if combined_offer_bundle.len() == 1
                    && !matches!(first_offer.source(), cm_rust::offer::OfferSource::Collection(_))
                {
                    extend_dict_with_offer::<DirConnector, _>(
                        component,
                        &child_component_output_dictionary_routers,
                        &component_input,
                        &program_output_dict,
                        framework_router,
                        &capability_sourced_capabilities_dict,
                        first_offer,
                        &target_input.capabilities(),
                        error_reporter.clone(),
                    )
                } else {
                    let aggregate_router = new_aggregate_router_from_service_offers(
                        &combined_offer_bundle,
                        component,
                        &child_component_output_dictionary_routers,
                        &component_input,
                        &program_output_dict,
                        framework_router,
                        &capability_sourced_capabilities_dict,
                        error_reporter.clone(),
                        aggregate_router_fn,
                    );
                    let prev = target_input
                        .capabilities()
                        .insert(first_offer.target_name().clone(), aggregate_router.into());
                    assert!(prev.is_none(), "failed to insert capability into target dict");
                }
            }
            cm_rust::offer::OfferDecl::Config(_) => extend_dict_with_offer::<Data, _>(
                component,
                &child_component_output_dictionary_routers,
                component_input,
                program_output_dict,
                framework_router,
                capability_sourced_capabilities_dict,
                first_offer,
                &target_input.capabilities(),
                error_reporter.clone(),
            ),
            cm_rust::offer::OfferDecl::Dictionary(_) => extend_dict_with_offer::<Dictionary, _>(
                component,
                &child_component_output_dictionary_routers,
                component_input,
                program_output_dict,
                framework_router,
                capability_sourced_capabilities_dict,
                first_offer,
                &target_input.capabilities(),
                error_reporter.clone(),
            ),
            cm_rust::offer::OfferDecl::Directory(_) | cm_rust::offer::OfferDecl::Storage(_) => {
                extend_dict_with_offer::<DirConnector, _>(
                    component,
                    &child_component_output_dictionary_routers,
                    component_input,
                    program_output_dict,
                    framework_router,
                    capability_sourced_capabilities_dict,
                    first_offer,
                    &target_input.capabilities(),
                    error_reporter.clone(),
                )
            }
            cm_rust::offer::OfferDecl::Protocol(_)
            | cm_rust::offer::OfferDecl::Runner(_)
            | cm_rust::offer::OfferDecl::Resolver(_) => extend_dict_with_offer::<Connector, _>(
                component,
                &child_component_output_dictionary_routers,
                component_input,
                program_output_dict,
                framework_router,
                capability_sourced_capabilities_dict,
                first_offer,
                &target_input.capabilities(),
                error_reporter.clone(),
            ),
            _ => {}
        }
    }
}

pub fn is_supported_use(use_: &cm_rust::UseDecl) -> bool {
    matches!(
        use_,
        cm_rust::UseDecl::Config(_)
            | cm_rust::UseDecl::Protocol(_)
            | cm_rust::UseDecl::Runner(_)
            | cm_rust::UseDecl::Service(_)
            | cm_rust::UseDecl::Directory(_)
            | cm_rust::UseDecl::EventStream(_)
            | cm_rust::UseDecl::Dictionary(_)
            | cm_rust::UseDecl::Storage(_)
    )
}

// Add the `config_use` to the `program_input_dict`, so the component is able to
// access this configuration.
fn extend_dict_with_config_use<C: ComponentInstanceInterface + 'static>(
    component: &Arc<C>,
    child_component_output_dictionary_routers: &HashMap<ChildName, Arc<Router<Dictionary>>>,
    component_input: &ComponentInput,
    program_input: &ProgramInput,
    program_output_dict: &Arc<Dictionary>,
    config_use: &cm_rust::UseConfigurationDecl,
    error_reporter: impl ErrorReporter,
) {
    let moniker = component.moniker();
    let source_path = config_use.source_path();
    let porcelain_type = CapabilityTypeName::Config;
    let router: Arc<Router<Data>> = match config_use.source() {
        cm_rust::UseSource::Parent => {
            use_from_parent_router::<Data>(component_input, source_path.to_owned(), moniker)
        }
        cm_rust::UseSource::Self_ => program_output_dict.get_router_or_not_found::<Data>(
            &source_path,
            RoutingError::use_from_self_not_found(moniker, source_path.iter_segments().join("/")),
        ),
        cm_rust::UseSource::Child(child_name) => {
            let child_name = ChildName::parse(child_name).expect("invalid child name");
            let Some(child_component_output) =
                child_component_output_dictionary_routers.get(&child_name)
            else {
                panic!(
                    "use declaration in manifest for component {} has a source of a nonexistent child {}, this should be prevented by manifest validation",
                    moniker, child_name
                );
            };
            child_component_output.clone().lazy_get(
                source_path.to_owned(),
                RoutingError::use_from_child_expose_not_found(
                    &child_name,
                    &moniker,
                    config_use.source_name().clone(),
                ),
            )
        }
        // The following are not used with config capabilities.
        cm_rust::UseSource::Environment => return,
        cm_rust::UseSource::Debug => return,
        cm_rust::UseSource::Capability(_) => return,
        cm_rust::UseSource::Framework => return,
        cm_rust::UseSource::Collection(_) => return,
    };

    let availability = *config_use.availability();
    let prev = program_input.config().insert_capability(
        &config_use.target_name,
        Capability::DataRouter(
            router
                .with_porcelain_with_default(porcelain_type)
                .availability(availability)
                .target(component)
                .error_info(config_use)
                .error_reporter(error_reporter)
                .log_errors()
                .build(),
        ),
    );
    assert!(prev.is_none(), "failed to insert {}: preexisting value", config_use.target_name);
}

fn extend_dict_with_event_stream_uses<C: ComponentInstanceInterface + 'static>(
    component: &Arc<C>,
    component_input: &ComponentInput,
    program_input: &ProgramInput,
    uses: Vec<&cm_rust::UseDecl>,
    error_reporter: impl ErrorReporter,
    event_stream_use_router_fn: &EventStreamUseRouterFn<C>,
) {
    let use_event_stream_decls = uses.into_iter().map(|u| match u {
        cm_rust::UseDecl::EventStream(decl) => decl,
        _other_use => panic!("conflicting use types share target path, this should be prevented by manifest validation"),
    }).collect::<Vec<_>>();
    let moniker = component.moniker();
    let porcelain_type = CapabilityTypeName::EventStream;
    let target_path = use_event_stream_decls.first().unwrap().target_path.clone();
    for use_event_stream_decl in &use_event_stream_decls {
        assert_eq!(
            &use_event_stream_decl.source,
            &cm_rust::UseSource::Parent,
            "event streams can only be used from parent, anything else should be caught by \
            manifest validation",
        );
    }
    let routers = use_event_stream_decls
        .into_iter()
        .map(|use_event_stream_decl| {
            let source_path = use_event_stream_decl.source_path().to_owned();
            let mut router_builder =
                use_from_parent_router::<Dictionary>(component_input, source_path, &moniker)
                    .with_porcelain_with_default(porcelain_type)
                    .availability(use_event_stream_decl.availability)
                    .target(component)
                    .error_info(RouteRequestErrorInfo::from(use_event_stream_decl.as_ref()))
                    .error_reporter(error_reporter.clone())
                    .log_errors();
            if let Some(scope) = &use_event_stream_decl.scope {
                router_builder =
                    router_builder.event_stream_scope((component.moniker().clone(), scope.clone()));
            }
            let router = router_builder.build();
            let filter = use_event_stream_decl.filter.clone();
            EventStreamSourceRouter { router, filter }
        })
        .collect::<Vec<_>>();

    let router = event_stream_use_router_fn(component, routers);
    let prev = program_input
        .namespace()
        .insert_capability(&target_path, Capability::ConnectorRouter(router));
    assert!(prev.is_none(), "failed to insert {target_path}: preexisting value");
}

fn extend_dict_with_use<T, C: ComponentInstanceInterface + 'static>(
    component: &Arc<C>,
    child_component_output_dictionary_routers: &HashMap<ChildName, Arc<Router<Dictionary>>>,
    component_input: &ComponentInput,
    program_input: &ProgramInput,
    program_output_dict: &Arc<Dictionary>,
    framework_router: &Arc<Router<Dictionary>>,
    capability_sourced_capabilities_dict: &Arc<Dictionary>,
    use_: &cm_rust::UseDecl,
    error_reporter: impl ErrorReporter,
) where
    T: CapabilityBound,
    Arc<T>: TryFrom<Capability>,
    Router<T>: CapabilityBound,
    Arc<Router<T>>: TryFrom<Capability>,
    Capability: From<Arc<T>>,
    Capability: From<Arc<Router<T>>>,
{
    if !is_supported_use(use_) {
        return;
    }
    let moniker = component.moniker();
    if let cm_rust::UseDecl::Config(config) = use_ {
        return extend_dict_with_config_use(
            component,
            child_component_output_dictionary_routers,
            component_input,
            program_input,
            program_output_dict,
            config,
            error_reporter,
        );
    };

    let source_path = use_.source_path();
    let porcelain_type = CapabilityTypeName::from(use_);
    let router: Arc<Router<T>> = match use_.source() {
        cm_rust::UseSource::Parent => {
            use_from_parent_router::<T>(component_input, source_path.to_owned(), moniker)
        }
        cm_rust::UseSource::Self_ => program_output_dict.get_router_or_not_found::<T>(
            &source_path,
            RoutingError::use_from_self_not_found(moniker, source_path.iter_segments().join("/")),
        ),
        cm_rust::UseSource::Child(child_name) => {
            let child_name = ChildName::parse(child_name).expect("invalid child name");
            let Some(child_component_output) =
                child_component_output_dictionary_routers.get(&child_name)
            else {
                panic!(
                    "use declaration in manifest for component {} has a source of a nonexistent child {}, this should be prevented by manifest validation",
                    moniker, child_name
                );
            };
            child_component_output.clone().lazy_get(
                source_path.to_owned(),
                RoutingError::use_from_child_expose_not_found(
                    &child_name,
                    &moniker,
                    use_.source_name().clone(),
                ),
            )
        }
        cm_rust::UseSource::Framework if use_.is_from_dictionary() => {
            Router::<T>::new_error(RoutingError::capability_from_framework_not_found(
                moniker,
                source_path.iter_segments().join("/"),
            ))
        }
        cm_rust::UseSource::Framework => {
            query_framework_router_or_not_found(framework_router, &source_path, component)
        }
        cm_rust::UseSource::Capability(capability_name) => {
            let err = RoutingError::capability_from_capability_not_found(
                moniker,
                capability_name.as_str().to_string(),
            );
            if source_path.iter_segments().join("/") == fsys::StorageAdminMarker::PROTOCOL_NAME
                || source_path.iter_segments().join("/")
                    == fcomponent::StorageAdminMarker::PROTOCOL_NAME
            {
                capability_sourced_capabilities_dict.get_router_or_not_found(&capability_name, err)
            } else {
                Router::<T>::new_error(err)
            }
        }
        cm_rust::UseSource::Debug => {
            let cm_rust::UseDecl::Protocol(use_protocol) = use_ else {
                panic!(
                    "non-protocol capability used with a debug source, this should be prevented by manifest validation"
                );
            };
            component_input.environment().debug().get_router_or_not_found::<T>(
                &use_protocol.source_name,
                RoutingError::use_from_environment_not_found(
                    moniker,
                    "protocol",
                    &use_protocol.source_name,
                ),
            )
        }
        cm_rust::UseSource::Environment => {
            let cm_rust::UseDecl::Runner(use_runner) = use_ else {
                panic!(
                    "non-runner capability used with an environment source, this should be prevented by manifest validation"
                );
            };
            component_input.environment().runners().get_router_or_not_found::<T>(
                &use_runner.source_name,
                RoutingError::use_from_environment_not_found(
                    moniker,
                    "runner",
                    &use_runner.source_name,
                ),
            )
        }
        cm_rust::UseSource::Collection(_) => {
            // Collection sources are handled separately, in `build_component_sandbox`
            return;
        }
    };

    let availability = *use_.availability();
    let mut router_builder = router
        .with_porcelain_with_default(porcelain_type)
        .availability(availability)
        .target(&component)
        .error_info(use_)
        .error_reporter(error_reporter)
        .log_errors()
        .with_tracing();
    if let cm_rust::UseDecl::Directory(decl) = use_ {
        router_builder = router_builder
            .rights(Some(decl.rights.into()))
            .subdir(decl.subdir.clone().into())
            .inherit_rights(false);
    }
    if let cm_rust::UseDecl::Service(_) = use_ {
        router_builder = router_builder.rights(Some(fio::R_STAR_DIR.into())).inherit_rights(false);
    }
    if let cm_rust::UseDecl::Storage(_) = use_ {
        router_builder = router_builder
            .rights(Some(fidl_fuchsia_io::RW_STAR_DIR.into()))
            .subdir(cm_types::RelativePath::dot().into())
            .inherit_rights(false);
    }
    let router = router_builder.build();

    match use_ {
        cm_rust::UseDecl::Protocol(cm_rust::UseProtocolDecl {
            numbered_handle: Some(numbered_handle),
            ..
        }) => {
            let numbered_handle = Name::from(*numbered_handle);
            let prev =
                program_input.numbered_handles().insert_capability(&numbered_handle, router.into());
            assert!(prev.is_none(), "failed to insert {numbered_handle}: preexisting value");
        }
        cm_rust::UseDecl::Runner(_) => {
            assert!(program_input.runner().is_none(), "component can't use multiple runners");
            program_input.set_runner(router.into());
        }
        _ => {
            let prev =
                program_input.namespace().insert_capability(use_.path().unwrap(), router.into());
            assert!(prev.is_none(), "failed to insert {}: preexisting value", use_.path().unwrap());
        }
    }
}

fn extend_dict_with_dictionary_use<C: ComponentInstanceInterface + 'static>(
    component: &Arc<C>,
    child_component_output_dictionary_routers: &HashMap<ChildName, Arc<Router<Dictionary>>>,
    component_input: &ComponentInput,
    program_input: &ProgramInput,
    program_output_dict: &Arc<Dictionary>,
    framework_router: &Arc<Router<Dictionary>>,
    capability_sourced_capabilities_dict: &Arc<Dictionary>,
    use_bundle: Vec<&cm_rust::UseDecl>,
    error_reporter: impl ErrorReporter,
) {
    let path = use_bundle[0].path().unwrap();
    let mut dictionary_routers = vec![];
    for use_ in use_bundle.iter() {
        let dict_for_used_router = ProgramInput::new(Dictionary::new(), None, Dictionary::new());
        extend_dict_with_use::<Dictionary, _>(
            component,
            child_component_output_dictionary_routers,
            component_input,
            &dict_for_used_router,
            program_output_dict,
            framework_router,
            capability_sourced_capabilities_dict,
            use_,
            error_reporter.clone(),
        );
        let dictionary_router = match dict_for_used_router.namespace().get_capability(path) {
            Some(Capability::DictionaryRouter(router)) => router,
            other_value => panic!("unexpected dictionary get result: {other_value:?}"),
        };
        dictionary_routers.push(dictionary_router);
    }
    let original_dictionary = match program_input.namespace().get_capability(path) {
        Some(Capability::Dictionary(dictionary)) => dictionary,
        _ => Dictionary::new(),
    };
    let router = UseDictionaryRouter::new(
        path.clone(),
        component.moniker().clone(),
        original_dictionary,
        dictionary_routers,
        CapabilitySource::Component(ComponentSource {
            capability: ComponentCapability::Use_((*use_bundle.first().unwrap()).clone()),
            moniker: component.moniker().clone(),
        }),
    );
    // This value will be `Some` if we're shadowing something else. This is fine in this case
    // because we've already merged any preexisting value with what we're inserting.
    let _ = program_input.namespace().insert_capability(path, Capability::DictionaryRouter(router));
}

/// Builds a router that obtains a capability that the program uses from `parent`.
fn use_from_parent_router<T>(
    component_input: &ComponentInput,
    source_path: impl IterablePath + 'static + Debug,
    moniker: &Moniker,
) -> Arc<Router<T>>
where
    T: CapabilityBound,
    Arc<T>: TryFrom<Capability>,
    Arc<Router<T>>: TryFrom<Capability>,
    Capability: From<Arc<T>>,
    Capability: From<Arc<Router<T>>>,
    Router<T>: CapabilityBound,
{
    let err = if moniker == &Moniker::root() {
        RoutingError::register_from_component_manager_not_found(
            source_path.iter_segments().join("/"),
        )
    } else {
        RoutingError::use_from_parent_not_found(moniker, source_path.iter_segments().join("/"))
    };
    component_input.capabilities().get_router_or_not_found::<T>(&source_path, err)
}

fn is_supported_offer(offer: &cm_rust::offer::OfferDecl) -> bool {
    matches!(
        offer,
        cm_rust::offer::OfferDecl::Config(_)
            | cm_rust::offer::OfferDecl::Protocol(_)
            | cm_rust::offer::OfferDecl::Dictionary(_)
            | cm_rust::offer::OfferDecl::Directory(_)
            | cm_rust::offer::OfferDecl::Runner(_)
            | cm_rust::offer::OfferDecl::Resolver(_)
            | cm_rust::offer::OfferDecl::Service(_)
            | cm_rust::offer::OfferDecl::EventStream(_)
            | cm_rust::offer::OfferDecl::Storage(_)
    )
}

fn extend_dict_with_offer<T, C: ComponentInstanceInterface + 'static>(
    component: &Arc<C>,
    child_component_output_dictionary_routers: &HashMap<ChildName, Arc<Router<Dictionary>>>,
    component_input: &ComponentInput,
    program_output_dict: &Arc<Dictionary>,
    framework_router: &Arc<Router<Dictionary>>,
    capability_sourced_capabilities_dict: &Arc<Dictionary>,
    offer: &cm_rust::offer::OfferDecl,
    target_dict: &Arc<Dictionary>,
    error_reporter: impl ErrorReporter,
) where
    T: CapabilityBound,
    Router<T>: CapabilityBound,
    Arc<T>: TryFrom<Capability>,
    Arc<Router<T>>: WithServiceRenamesAndFilter,
    Arc<Router<T>>: TryFrom<Capability>,
    Capability: From<Arc<T>>,
    Capability: From<Arc<Router<T>>>,
{
    assert!(is_supported_offer(offer), "{offer:?}");

    let source_path = offer.source_path();
    let target_name = offer.target_name();
    let porcelain_type = CapabilityTypeName::from(offer);
    let router: Arc<Router<T>> = match offer.source() {
        cm_rust::offer::OfferSource::Parent => {
            let err = if component.moniker() == &Moniker::root() {
                RoutingError::register_from_component_manager_not_found(
                    offer.source_name().to_string(),
                )
            } else {
                RoutingError::offer_from_parent_not_found(
                    &component.moniker(),
                    source_path.iter_segments().join("/"),
                )
            };
            component_input.capabilities().get_router_or_not_found::<T>(&source_path, err)
        }
        cm_rust::offer::OfferSource::Self_ => program_output_dict.get_router_or_not_found::<T>(
            &source_path,
            RoutingError::offer_from_self_not_found(
                &component.moniker(),
                source_path.iter_segments().join("/"),
            ),
        ),
        cm_rust::offer::OfferSource::Child(child_ref) => {
            let child_name: ChildName = child_ref.clone().try_into().expect("invalid child ref");
            match child_component_output_dictionary_routers.get(&child_name) {
                None => Router::<T>::new_error(RoutingError::offer_from_child_instance_not_found(
                    &child_name,
                    &component.moniker(),
                    source_path.iter_segments().join("/"),
                )),
                Some(child_component_output) => child_component_output.clone().lazy_get(
                    source_path.to_owned(),
                    RoutingError::offer_from_child_expose_not_found(
                        &child_name,
                        &component.moniker(),
                        offer.source_name().clone(),
                    ),
                ),
            }
        }
        cm_rust::offer::OfferSource::Framework => {
            if offer.is_from_dictionary() {
                warn!(
                    "routing from framework with dictionary path is not supported: {source_path}"
                );
                return;
            }
            query_framework_router_or_not_found(framework_router, &source_path, component)
        }
        cm_rust::offer::OfferSource::Capability(capability_name) => {
            let err = RoutingError::capability_from_capability_not_found(
                &component.moniker(),
                capability_name.as_str().to_string(),
            );
            if source_path.iter_segments().join("/") == fsys::StorageAdminMarker::PROTOCOL_NAME
                || source_path.iter_segments().join("/")
                    == fcomponent::StorageAdminMarker::PROTOCOL_NAME
            {
                capability_sourced_capabilities_dict.get_router_or_not_found(&capability_name, err)
            } else {
                Router::<T>::new_error(err)
            }
        }
        cm_rust::offer::OfferSource::Void => UnavailableRouter::new_from_offer(offer, component),
        cm_rust::offer::OfferSource::Collection(_collection_name) => {
            // There's nothing in a collection at this stage, and thus we can't get any routers to
            // things in the collection. What's more: the contents of the collection can change
            // over time, so it must be monitored. We don't handle collections here, they're
            // handled in a different way by whoever called `extend_dict_with_offer`.
            return;
        }
    };

    let availability = *offer.availability();
    let mut router_builder = router
        .with_porcelain_with_default(porcelain_type)
        .availability(availability)
        .target(component)
        .error_info(offer)
        .error_reporter(error_reporter);
    if let cm_rust::offer::OfferDecl::Directory(decl) = offer {
        // Offered capabilities need to support default requests in the case of
        // offer-to-dictionary. This is a corollary of the fact that program_input_dictionary and
        // component_output_dictionary support default requests, and we need this to cover the case
        // where the use or expose is from a dictionary.
        //
        // Technically, we could restrict this to the case of offer-to-dictionary, not offer in
        // general. However, supporting the general case simplifies the logic and establishes a
        // nice symmetry between program_input_dict, component_output_dict, and
        // {child,collection}_inputs.
        router_builder = router_builder
            .rights(decl.rights.clone().map(Into::into))
            .subdir(decl.subdir.clone().into())
            .inherit_rights(true);
    }
    if let cm_rust::offer::OfferDecl::Storage(_) = offer {
        router_builder = router_builder
            .rights(Some(fio::RW_STAR_DIR.into()))
            .inherit_rights(false)
            .subdir(cm_types::RelativePath::dot().into());
    }
    if let cm_rust::offer::OfferDecl::Service(_) = offer {
        router_builder = router_builder.rights(Some(fio::R_STAR_DIR.into())).inherit_rights(true);
    }
    if let cm_rust::offer::OfferDecl::EventStream(offer_event_stream) = offer {
        if let Some(scope) = &offer_event_stream.scope {
            router_builder =
                router_builder.event_stream_scope((component.moniker().clone(), scope.clone()));
        }
    }

    let router = if let cm_rust::OfferDecl::Service(_) = offer {
        router_builder.build().with_service_renames_and_filter(offer.clone())
    } else {
        router_builder.build().into()
    };

    let prev = target_dict.insert_capability(target_name, router.into());
    assert!(prev.is_none(), "failed to insert {target_name}: preexisting value");
}

fn query_framework_router_or_not_found<T, C>(
    router: &Arc<Router<Dictionary>>,
    path: &BorrowedSeparatedPath<'_>,
    component: &Arc<C>,
) -> Arc<Router<T>>
where
    T: CapabilityBound,
    Router<T>: CapabilityBound,
    Arc<Router<T>>: TryFrom<Capability>,
    Arc<T>: TryFrom<Capability>,
    Capability: From<Arc<T>>,
    Capability: From<Arc<Router<T>>>,
    C: ComponentInstanceInterface + 'static,
{
    let dict: Result<Option<Arc<Dictionary>>, RouterError> = router
        .route(RouteRequest::default(), component.as_weak().into())
        .now_or_never()
        .expect("failed to now_or_never");
    let dict = match dict {
        Ok(Some(dict)) => dict,
        // shouldn't happen, fallback
        _ => Dictionary::new(),
    };
    // `lazy_get` is not needed here as the framework dictionary does not contain
    // dictionary routers that have to be queried in turn.
    dict.get_router_or_not_found::<T>(
        path,
        RoutingError::capability_from_framework_not_found(
            &component.moniker(),
            path.iter_segments().join("/"),
        ),
    )
}

pub fn is_supported_expose(expose: &cm_rust::ExposeDecl) -> bool {
    matches!(
        expose,
        cm_rust::ExposeDecl::Config(_)
            | cm_rust::ExposeDecl::Protocol(_)
            | cm_rust::ExposeDecl::Dictionary(_)
            | cm_rust::ExposeDecl::Directory(_)
            | cm_rust::ExposeDecl::Runner(_)
            | cm_rust::ExposeDecl::Resolver(_)
            | cm_rust::ExposeDecl::Service(_)
    )
}

fn extend_dict_with_expose<T, C: ComponentInstanceInterface + 'static>(
    component: &Arc<C>,
    child_component_output_dictionary_routers: &HashMap<ChildName, Arc<Router<Dictionary>>>,
    program_output_dict: &Arc<Dictionary>,
    framework_router: &Arc<Router<Dictionary>>,
    capability_sourced_capabilities_dict: &Arc<Dictionary>,
    expose: &cm_rust::ExposeDecl,
    target_component_output: &ComponentOutput,
    error_reporter: impl ErrorReporter,
) where
    T: CapabilityBound,
    Router<T>: CapabilityBound,
    Arc<T>: TryFrom<Capability>,
    Arc<Router<T>>: TryFrom<Capability>,
    Capability: From<Arc<T>>,
    Capability: From<Arc<Router<T>>>,
{
    assert!(is_supported_expose(expose), "{expose:?}");

    let target_dict = match expose.target() {
        cm_rust::ExposeTarget::Parent => target_component_output.capabilities(),
        cm_rust::ExposeTarget::Framework => target_component_output.framework(),
    };
    let source_path = expose.source_path();
    let target_name = expose.target_name();

    let porcelain_type = CapabilityTypeName::from(expose);
    let router: Arc<Router<T>> = match expose.source() {
        cm_rust::ExposeSource::Self_ => program_output_dict.get_router_or_not_found::<T>(
            &source_path,
            RoutingError::expose_from_self_not_found(
                &component.moniker(),
                source_path.iter_segments().join("/"),
            ),
        ),
        cm_rust::ExposeSource::Child(child_name) => {
            let child_name = ChildName::parse(child_name).expect("invalid static child name");
            if let Some(child_component_output) =
                child_component_output_dictionary_routers.get(&child_name)
            {
                child_component_output.clone().lazy_get(
                    source_path.to_owned(),
                    RoutingError::expose_from_child_expose_not_found(
                        &child_name,
                        &component.moniker(),
                        expose.source_name().clone(),
                    ),
                )
            } else {
                Router::<T>::new_error(RoutingError::expose_from_child_instance_not_found(
                    &child_name,
                    &component.moniker(),
                    expose.source_name().clone(),
                ))
            }
        }
        cm_rust::ExposeSource::Framework => {
            if expose.is_from_dictionary() {
                warn!(
                    "routing from framework with dictionary path is not supported: {source_path}"
                );
                return;
            }
            query_framework_router_or_not_found(framework_router, &source_path, component)
        }
        cm_rust::ExposeSource::Capability(capability_name) => {
            let err = RoutingError::capability_from_capability_not_found(
                &component.moniker(),
                capability_name.as_str().to_string(),
            );
            if source_path.iter_segments().join("/") == fsys::StorageAdminMarker::PROTOCOL_NAME
                || source_path.iter_segments().join("/")
                    == fcomponent::StorageAdminMarker::PROTOCOL_NAME
            {
                capability_sourced_capabilities_dict
                    .clone()
                    .get_router_or_not_found::<T>(&capability_name, err)
            } else {
                Router::<T>::new_error(err)
            }
        }
        cm_rust::ExposeSource::Void => UnavailableRouter::new_from_expose(expose, component),
        // There's nothing in a collection at this stage, and thus we can't get any routers to
        // things in the collection. What's more: the contents of the collection can change over
        // time, so it must be monitored. We don't handle collections here, they're handled in a
        // different way by whoever called `extend_dict_with_expose`.
        cm_rust::ExposeSource::Collection(_name) => return,
    };
    let availability = *expose.availability();
    let mut router_builder = router
        .with_porcelain_with_default(porcelain_type)
        .availability(availability)
        .target(component)
        .error_info(expose)
        .error_reporter(error_reporter);
    if let cm_rust::ExposeDecl::Directory(decl) = expose {
        router_builder = router_builder
            .rights(decl.rights.clone().map(Into::into))
            .subdir(decl.subdir.clone().into())
            .inherit_rights(true);
    };
    if let cm_rust::ExposeDecl::Service(_) = expose {
        router_builder = router_builder.rights(Some(fio::R_STAR_DIR.into())).inherit_rights(true);
    };
    let prev = target_dict.insert_capability(target_name, router_builder.build().into());
    assert!(prev.is_none(), "failed to insert {target_name}: preexisting value");
}

struct UnavailableRouter<C: ComponentInstanceInterface> {
    capability: InternalCapability,
    component: WeakComponentInstanceInterface<C>,
}

impl<C: ComponentInstanceInterface + 'static> UnavailableRouter<C> {
    fn new<T: CapabilityBound>(
        capability: InternalCapability,
        component: &Arc<C>,
    ) -> Arc<Router<T>> {
        Router::<T>::new(Self { capability, component: component.as_weak() })
    }

    fn new_from_offer<T: CapabilityBound>(offer: &OfferDecl, component: &Arc<C>) -> Arc<Router<T>> {
        let name = offer.source_name().clone();
        let capability = match offer {
            OfferDecl::Service(_) => InternalCapability::Service(name),
            OfferDecl::Protocol(_) => InternalCapability::Protocol(name),
            OfferDecl::Directory(_) => InternalCapability::Directory(name),
            OfferDecl::Storage(_) => InternalCapability::Storage(name),
            OfferDecl::Runner(_) => InternalCapability::Runner(name),
            OfferDecl::Resolver(_) => InternalCapability::Resolver(name),
            OfferDecl::EventStream(_) => {
                InternalCapability::EventStream(InternalEventStreamCapability {
                    name,
                    scope_moniker: None,
                    scope: None,
                })
            }
            OfferDecl::Dictionary(_) => InternalCapability::Dictionary(name),
            OfferDecl::Config(_) => InternalCapability::Config(name),
        };
        Self::new(capability, component)
    }

    fn new_from_expose<T: CapabilityBound>(
        expose: &ExposeDecl,
        component: &Arc<C>,
    ) -> Arc<Router<T>> {
        let name = expose.source_name().clone();
        let capability = match expose {
            ExposeDecl::Service(_) => InternalCapability::Service(name),
            ExposeDecl::Protocol(_) => InternalCapability::Protocol(name),
            ExposeDecl::Directory(_) => InternalCapability::Directory(name),
            ExposeDecl::Runner(_) => InternalCapability::Runner(name),
            ExposeDecl::Resolver(_) => InternalCapability::Resolver(name),
            ExposeDecl::Dictionary(_) => InternalCapability::Dictionary(name),
            ExposeDecl::Config(_) => InternalCapability::Config(name),
        };
        Self::new(capability, component)
    }
}

#[async_trait]
impl<T: CapabilityBound, C: ComponentInstanceInterface + 'static> Routable<T>
    for UnavailableRouter<C>
{
    async fn route(
        &self,
        request: RouteRequest,
        _target: Arc<WeakInstanceToken>,
    ) -> Result<Option<Arc<T>>, RouterError> {
        let availability = request.availability.ok_or(RouterError::InvalidArgs)?.fidl_into_native();
        match availability {
            cm_rust::Availability::Required | cm_rust::Availability::SameAsTarget => {
                Err(RoutingError::SourceCapabilityIsVoid {
                    moniker: self.component.moniker.clone(),
                }
                .into())
            }
            cm_rust::Availability::Optional | cm_rust::Availability::Transitional => Ok(None),
        }
    }

    async fn route_debug(
        &self,
        _request: RouteRequest,
        _target: Arc<WeakInstanceToken>,
    ) -> Result<CapabilitySource, RouterError> {
        Ok(CapabilitySource::Void(VoidSource {
            capability: self.capability.clone(),
            moniker: self.component.moniker.clone(),
        }))
    }
}
