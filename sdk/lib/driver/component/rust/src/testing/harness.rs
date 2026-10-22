// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The harness provides a way to spin up drivers for unit testing.

pub use crate::testing::dut::DriverUnderTest;
use crate::testing::logsink_connector;
use crate::testing::node::NodeManager;
use crate::{Driver, Incoming, PropertyValue};
use anyhow::Result;
use fdf::{AsyncDispatcher, DispatcherBuilder};
use fdf_env::Environment;
use fidl::endpoints::{ClientEnd, Proxy};
use fidl_fuchsia_driver_framework::Offer;
use fidl_fuchsia_io as fio;
use fidl_next::{ClientEnd as NextClientEnd, ServerEnd as NextServerEnd};
use fidl_next_fuchsia_component_runner::natural::ComponentNamespaceEntry;
use fidl_next_fuchsia_driver_framework as df_next;
use fidl_next_fuchsia_driver_framework::DriverStartArgs;
use fidl_next_fuchsia_driver_framework::natural::Offer as NextOffer;
use fuchsia_async as fasync;
use fuchsia_component::directory::open_directory_async;
use fuchsia_component::server::{ServiceFs, ServiceObj};
use futures::StreamExt;
use std::marker::PhantomData;
use std::sync::{Arc, Weak, mpsc};
use zx::Status;

/// The main test harness for running a driver unit test.
pub struct TestHarness<D> {
    fdf_env_environment: Arc<Environment>,
    node_manager: Arc<NodeManager>,
    driver: Option<fdf_env::Driver<u32>>,
    dispatcher: AsyncDispatcher,
    driver_incoming_dir: ClientEnd<fio::DirectoryMarker>,
    config_vmo: Option<zx::Vmo>,
    url: Option<String>,
    node_name: Option<String>,
    offers: Option<Vec<NextOffer>>,
    scope: fasync::Scope,
    power_element_args: Option<df_next::PowerElementArgs>,
    node_token: Option<zx::Event>,
    node_properties: Vec<df_next::NodePropertyEntry2>,
    _d: PhantomData<D>,
}

impl<D: Driver> Default for TestHarness<D> {
    fn default() -> Self {
        Self::new()
    }
}

impl<D: Driver> TestHarness<D> {
    /// Creates a new `TestHarness` without a customized driver incoming ServiceFs.
    pub fn new() -> Self {
        let scope = fasync::Scope::new();
        let mut driver_incoming = ServiceFs::new();
        let env = Arc::new(Environment::start(0).unwrap());
        let node_manager = NodeManager::new();
        driver_incoming.dir("svc").add_service_connector(logsink_connector);

        let (driver_incoming_dir_client, driver_incoming_dir_server) = zx::Channel::create();
        driver_incoming.serve_connection(driver_incoming_dir_server.into()).unwrap();
        let driver_incoming_dir = driver_incoming_dir_client.into();

        scope.spawn(async move {
            driver_incoming.collect::<()>().await;
        });

        // Leak this to a raw, we will reconstitue a Box inside drop.
        let driver_value_ptr = Box::into_raw(Box::new(0x1234_u32));
        let driver = env.new_driver(driver_value_ptr);
        let env_clone = env.clone();
        let dispatcher_builder =
            DispatcherBuilder::new().name("test_harness").shutdown_observer(move |dispatcher| {
                // We verify that the dispatcher has no tasks left queued in it,
                // just because this is testing code.
                assert!(!env_clone.dispatcher_has_queued_tasks(dispatcher.as_dispatcher_ref()));
            });
        let dispatcher =
            AsyncDispatcher::new(&driver.new_dispatcher(dispatcher_builder).unwrap().release());
        let driver = Some(driver);

        Self {
            fdf_env_environment: env,
            node_manager,
            driver,
            dispatcher,
            driver_incoming_dir,
            config_vmo: None,
            url: None,
            node_name: None,
            offers: None,
            scope,
            power_element_args: None,
            node_token: None,
            node_properties: Vec::new(),
            _d: PhantomData,
        }
    }

    /// Sets the driver incoming ServiceFs. Consumes and returns self to allow chaining.
    pub fn set_driver_incoming(
        mut self,
        mut driver_incoming: ServiceFs<ServiceObj<'static, ()>>,
    ) -> Self {
        driver_incoming.dir("svc").add_service_connector(logsink_connector);

        let (driver_incoming_dir_client, driver_incoming_dir_server) = zx::Channel::create();
        driver_incoming.serve_connection(driver_incoming_dir_server.into()).unwrap();
        let driver_incoming_dir = driver_incoming_dir_client.into();
        self.scope.spawn(async move {
            driver_incoming.collect::<()>().await;
        });

        self.driver_incoming_dir = driver_incoming_dir;
        self
    }

    /// Sets the configuration vmo for the driver. Consumes and returns self to allow chaining.
    pub fn set_config(mut self, config: zx::Vmo) -> Self {
        self.config_vmo = Some(config);
        self
    }

    /// Sets the url for the driver. Consumes and returns self to allow chaining.
    pub fn set_url(mut self, url: &str) -> Self {
        self.url = Some(url.to_string());
        self
    }

    /// Sets the node name for the driver. Consumes and returns self to allow chaining.
    pub fn set_node_name(mut self, name: impl Into<String>) -> Self {
        self.node_name = Some(name.into());
        self
    }

    /// Adds an offer to the driver's start args. Consumes and returns self to allow chaining.
    pub fn add_offer(mut self, offer: Offer) -> Self {
        self.offers.get_or_insert_default().push(convert::convert_df_offer(offer));
        self
    }

    /// Sets the power element args for the driver. Accepts both
    /// `fidl_next_fuchsia_driver_framework::PowerElementArgs` and, for drivers that haven't
    /// migrated yet, the legacy `fidl_fuchsia_driver_framework::PowerElementArgs`. Consumes and
    /// returns self to allow chaining.
    pub fn set_power_element_args(mut self, args: impl IntoPowerElementArgs) -> Self {
        self.power_element_args = Some(args.into_power_element_args());
        self
    }

    /// Sets the node token for the driver. Consumes and returns self to allow chaining.
    pub fn set_node_token(mut self, token: zx::Event) -> Self {
        self.node_token = Some(token);
        self
    }

    /// Sets the power element dependency token for the driver, as returned by
    /// [`crate::DriverContext::take_power_element_token`]. This only populates the `token` of the
    /// power element args; use [`Self::set_power_element_args`] to also provide the power broker
    /// endpoints. Consumes and returns self to allow chaining.
    pub fn set_power_element_token(mut self, token: zx::Event) -> Self {
        self.power_element_args.get_or_insert_default().token = Some(token);
        self
    }

    /// Adds a property of the parent node named `parent` to the driver's start args, as reported
    /// by [`crate::DriverContext::node_properties`]. Drivers bound to a single node see their
    /// properties under the parent name `default`. Consumes and returns self to allow chaining.
    pub fn add_node_property(
        mut self,
        parent: &str,
        key: impl Into<String>,
        value: impl Into<PropertyValue>,
    ) -> Self {
        let property = df_next::NodeProperty2 {
            key: key.into(),
            value: convert::convert_property_value(value.into()),
        };
        match self.node_properties.iter_mut().find(|entry| entry.name == parent) {
            Some(entry) => entry.properties.push(property),
            None => self.node_properties.push(df_next::NodePropertyEntry2 {
                name: parent.to_string(),
                properties: vec![property],
            }),
        }
        self
    }

    /// Gets a driver dispatcher that can be used to run test side driver transport client/servers.
    pub fn dispatcher(&self) -> AsyncDispatcher {
        self.dispatcher.clone()
    }

    pub(crate) fn node_manager(&self) -> Weak<NodeManager> {
        Arc::downgrade(&self.node_manager)
    }

    /// Starts the driver under test.
    pub async fn start_driver(&mut self) -> Result<DriverUnderTest<'_, D>, Status> {
        let (node_client, node_server) = zx::Channel::create();
        let node_id = self.node_manager.create_root_node(node_server.into());

        let (driver_outgoing_dir_client, driver_outgoing_dir_server) =
            fidl::endpoints::create_endpoints();
        let driver_outgoing = Incoming::from(driver_outgoing_dir_client);

        let driver_incoming_svc =
            open_directory_async(&self.driver_incoming_dir, ".", fio::R_STAR_DIR).unwrap();

        let start_args = DriverStartArgs {
            node: Some(NextClientEnd::from_untyped(node_client)),
            incoming: Some(vec![ComponentNamespaceEntry {
                path: Some("/".to_string()),
                directory: Some(NextClientEnd::from_untyped(
                    driver_incoming_svc.into_channel().unwrap().into(),
                )),
            }]),
            outgoing_dir: Some(NextServerEnd::from_untyped(
                driver_outgoing_dir_server.into_channel(),
            )),
            config: self
                .config_vmo
                .as_ref()
                .and_then(|v| v.duplicate_handle(fidl::Rights::SAME_RIGHTS).ok()),
            url: self.url.clone(),
            node_name: self.node_name.clone(),
            node_offers: self.offers.clone(),
            power_element_args: self.power_element_args.take(),
            node_token: self
                .node_token
                .as_ref()
                .and_then(|t| t.duplicate_handle(fidl::Rights::SAME_RIGHTS).ok()),
            node_properties_2: (!self.node_properties.is_empty())
                .then(|| self.node_properties.clone()),
            ..DriverStartArgs::default()
        };

        let mut driver =
            DriverUnderTest::new(self, self.fdf_env_environment.clone(), driver_outgoing, node_id)
                .await;
        // If the driver fails to start we will drop it here and allow it to run the destroy hook.
        driver.start_driver(start_args).await?;
        Ok(driver)
    }
}

impl<D> Drop for TestHarness<D> {
    fn drop(&mut self) {
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        self.driver.take().expect("driver").shutdown(move |driver_ref| {
            // SAFETY: we created this through Box::into_raw below inside of new.
            let driver_value = unsafe { Box::from_raw(driver_ref.0 as *mut u32) };
            assert_eq!(*driver_value, 0x1234);
            shutdown_tx.send(()).unwrap();
        });

        shutdown_rx.recv().unwrap();

        self.fdf_env_environment.destroy_all_dispatchers();
        self.fdf_env_environment.reset();
    }
}

/// Power element args accepted by [`TestHarness::set_power_element_args`].
///
/// Implemented for `fidl_next_fuchsia_driver_framework::PowerElementArgs`, which the harness
/// stores, and for the legacy `fidl_fuchsia_driver_framework::PowerElementArgs` so that drivers
/// still on the legacy bindings keep compiling while they migrate.
pub trait IntoPowerElementArgs {
    /// Converts the args into their `fidl_next` representation.
    fn into_power_element_args(self) -> df_next::PowerElementArgs;
}

impl IntoPowerElementArgs for df_next::PowerElementArgs {
    fn into_power_element_args(self) -> df_next::PowerElementArgs {
        self
    }
}

impl IntoPowerElementArgs for fidl_fuchsia_driver_framework::PowerElementArgs {
    fn into_power_element_args(self) -> df_next::PowerElementArgs {
        df_next::PowerElementArgs {
            control_client: self
                .control_client
                .map(|c| NextClientEnd::from_untyped(c.into_channel())),
            runner_server: self
                .runner_server
                .map(|s| NextServerEnd::from_untyped(s.into_channel())),
            lessor_client: self
                .lessor_client
                .map(|c| NextClientEnd::from_untyped(c.into_channel())),
            token: self.token,
        }
    }
}

mod convert {
    use fidl_fuchsia_component_decl as decl;
    use fidl_fuchsia_driver_framework as df;
    use fidl_next_fuchsia_component_decl as decl_next;
    use fidl_next_fuchsia_driver_framework as df_next;

    pub fn convert_property_value(value: crate::PropertyValue) -> df_next::NodePropertyValue {
        match value.0 {
            df::NodePropertyValue::IntValue(v) => df_next::NodePropertyValue::IntValue(v),
            df::NodePropertyValue::StringValue(v) => df_next::NodePropertyValue::StringValue(v),
            df::NodePropertyValue::BoolValue(v) => df_next::NodePropertyValue::BoolValue(v),
            df::NodePropertyValue::EnumValue(v) => df_next::NodePropertyValue::EnumValue(v),
            df::NodePropertyValue::__SourceBreaking { unknown_ordinal } => {
                df_next::NodePropertyValue::UnknownOrdinal_(unknown_ordinal)
            }
        }
    }

    pub fn convert_df_offer(offer: df::Offer) -> df_next::Offer {
        match offer {
            df::Offer::DictionaryOffer(o) => df_next::Offer::DictionaryOffer(convert_offer(o)),
            df::Offer::ZirconTransport(o) => df_next::Offer::ZirconTransport(convert_offer(o)),
            df::Offer::DriverTransport(o) => df_next::Offer::DriverTransport(convert_offer(o)),
            df::Offer::__SourceBreaking { unknown_ordinal } => {
                df_next::Offer::UnknownOrdinal_(unknown_ordinal)
            }
        }
    }

    fn convert_offer(offer: decl::Offer) -> decl_next::Offer {
        match offer {
            decl::Offer::Service(o) => decl_next::Offer::Service(decl_next::OfferService {
                source: o.source.map(convert_ref),
                source_name: o.source_name,
                target: o.target.map(convert_ref),
                target_name: o.target_name,
                source_instance_filter: o.source_instance_filter,
                renamed_instances: o
                    .renamed_instances
                    .map(|v| v.into_iter().map(convert_name_mapping).collect()),
                availability: o.availability.map(convert_availability),
                source_dictionary: o.source_dictionary,
                dependency_type: o.dependency_type.map(convert_dependency_type),
            }),
            decl::Offer::Protocol(o) => decl_next::Offer::Protocol(decl_next::OfferProtocol {
                source: o.source.map(convert_ref),
                source_name: o.source_name,
                target: o.target.map(convert_ref),
                target_name: o.target_name,
                dependency_type: o.dependency_type.map(convert_dependency_type),
                availability: o.availability.map(convert_availability),
                source_dictionary: o.source_dictionary,
            }),
            decl::Offer::Directory(o) => decl_next::Offer::Directory(decl_next::OfferDirectory {
                source: o.source.map(convert_ref),
                source_name: o.source_name,
                target: o.target.map(convert_ref),
                target_name: o.target_name,
                availability: o.availability.map(convert_availability),
                source_dictionary: o.source_dictionary,
                dependency_type: o.dependency_type.map(convert_dependency_type),
                rights: o.rights.map(convert_rights),
                subdir: o.subdir,
            }),
            decl::Offer::Storage(o) => decl_next::Offer::Storage(decl_next::OfferStorage {
                source_name: o.source_name,
                source: o.source.map(convert_ref),
                target: o.target.map(convert_ref),
                target_name: o.target_name,
                availability: o.availability.map(convert_availability),
            }),
            decl::Offer::Runner(o) => decl_next::Offer::Runner(decl_next::OfferRunner {
                source: o.source.map(convert_ref),
                source_name: o.source_name,
                target: o.target.map(convert_ref),
                target_name: o.target_name,
                source_dictionary: o.source_dictionary,
            }),
            decl::Offer::Resolver(o) => decl_next::Offer::Resolver(decl_next::OfferResolver {
                source: o.source.map(convert_ref),
                source_name: o.source_name,
                target: o.target.map(convert_ref),
                target_name: o.target_name,
                source_dictionary: o.source_dictionary,
            }),
            decl::Offer::EventStream(o) => {
                decl_next::Offer::EventStream(decl_next::OfferEventStream {
                    source: o.source.map(convert_ref),
                    source_name: o.source_name,
                    scope: o.scope.map(|v| v.into_iter().map(convert_ref).collect()),
                    target: o.target.map(convert_ref),
                    target_name: o.target_name,
                    availability: o.availability.map(convert_availability),
                })
            }
            decl::Offer::Dictionary(o) => {
                decl_next::Offer::Dictionary(decl_next::OfferDictionary {
                    source: o.source.map(convert_ref),
                    source_name: o.source_name,
                    target: o.target.map(convert_ref),
                    target_name: o.target_name,
                    dependency_type: o.dependency_type.map(convert_dependency_type),
                    availability: o.availability.map(convert_availability),
                    source_dictionary: o.source_dictionary,
                })
            }
            decl::Offer::Config(o) => decl_next::Offer::Config(decl_next::OfferConfiguration {
                source: o.source.map(convert_ref),
                source_name: o.source_name,
                target: o.target.map(convert_ref),
                target_name: o.target_name,
                availability: o.availability.map(convert_availability),
                source_dictionary: o.source_dictionary,
            }),
            decl::Offer::__SourceBreaking { unknown_ordinal } => {
                decl_next::Offer::UnknownOrdinal_(unknown_ordinal)
            }
        }
    }

    fn convert_ref(ref_: decl::Ref) -> decl_next::Ref {
        match ref_ {
            decl::Ref::Parent(_) => decl_next::Ref::Parent(()),
            decl::Ref::Self_(_) => decl_next::Ref::Self_(()),
            decl::Ref::Child(child_ref) => decl_next::Ref::Child(decl_next::ChildRef {
                name: child_ref.name,
                collection: child_ref.collection,
            }),
            decl::Ref::Collection(collection_ref) => {
                decl_next::Ref::Collection(decl_next::CollectionRef { name: collection_ref.name })
            }
            decl::Ref::Framework(_) => decl_next::Ref::Framework(()),
            decl::Ref::Capability(capability_ref) => {
                decl_next::Ref::Capability(decl_next::CapabilityRef { name: capability_ref.name })
            }
            decl::Ref::Debug(_) => decl_next::Ref::Debug(()),
            decl::Ref::VoidType(_) => decl_next::Ref::VoidType(()),
            decl::Ref::Environment(_) => decl_next::Ref::Environment(()),
            decl::Ref::__SourceBreaking { unknown_ordinal } => {
                decl_next::Ref::UnknownOrdinal_(unknown_ordinal)
            }
        }
    }

    fn convert_name_mapping(name_mapping: decl::NameMapping) -> decl_next::NameMapping {
        fidl_next_fuchsia_component_decl::NameMapping {
            source_name: name_mapping.source_name,
            target_name: name_mapping.target_name,
        }
    }

    fn convert_availability(availability: decl::Availability) -> decl_next::Availability {
        match availability {
            decl::Availability::Required => decl_next::Availability::Required,
            decl::Availability::Optional => decl_next::Availability::Optional,
            decl::Availability::SameAsTarget => decl_next::Availability::SameAsTarget,
            decl::Availability::Transitional => decl_next::Availability::Transitional,
        }
    }

    fn convert_dependency_type(dependency_type: decl::DependencyType) -> decl_next::DependencyType {
        match dependency_type {
            decl::DependencyType::Strong => decl_next::DependencyType::Strong,
            decl::DependencyType::Weak => decl_next::DependencyType::Weak,
        }
    }

    fn convert_rights(rights: fidl_fuchsia_io::Operations) -> fidl_next_fuchsia_io::Operations {
        fidl_next_fuchsia_io::Operations::from_bits_retain(rights.bits())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DriverError, Node, NodeBuilder, ServiceInstance, ServiceOffer};
    use fidl_next::{Request, Responder};
    use fidl_next_fuchsia_examples as fexample;
    use fidl_next_fuchsia_examples::echo::{EchoString, SendString};
    use fuchsia_async as fasync;
    use futures::StreamExt;
    use futures::lock::Mutex;
    use log::info;

    struct EchoServer;

    impl fexample::EchoServerHandler<zx::Channel> for EchoServer {
        async fn echo_string(
            &mut self,
            request: Request<EchoString, zx::Channel>,
            responder: Responder<EchoString, zx::Channel>,
        ) {
            info!("ECHO: {}", request.payload().value);
            responder.respond("resp").await.unwrap();
        }

        async fn send_string(&mut self, _request: Request<SendString, zx::Channel>) {}
    }

    struct Service {
        scope: fasync::ScopeHandle,
    }

    impl fexample::EchoServiceHandler for Service {
        fn regular_echo(&self, server_end: NextServerEnd<fexample::Echo>) {
            server_end.spawn_on(EchoServer, &self.scope);
        }

        fn reversed_echo(&self, _server_end: NextServerEnd<fexample::Echo>) {}
    }

    #[allow(dead_code)]
    struct TestDriver {
        node: Node,
        scope: fasync::Scope,
        tmp: Mutex<String>,
    }

    impl TestDriver {
        async fn set_tmp(&self, resp: &str) {
            let mut tmp = self.tmp.lock().await;
            *tmp = resp.to_string();
        }

        async fn get_tmp(&self) -> String {
            let tmp = self.tmp.lock().await;
            tmp.to_string()
        }
    }

    impl Driver for TestDriver {
        const NAME: &'static str = "test-driver";

        async fn start(mut context: crate::DriverContext) -> Result<Self, DriverError> {
            let service_proxy: ServiceInstance<fexample::EchoService> =
                context.incoming.service().connect_next()?;
            let (client_end, server_end) = fidl_next::fuchsia::create_channel();
            service_proxy.regular_echo(server_end).unwrap();
            let client = client_end.spawn();
            let resp =
                client.echo_string("echo from driver").await.map_err(|_| Status::IO_REFUSED)?;
            assert_eq!("resp", resp.response.as_str());

            let scope = fasync::Scope::new_with_name("test driver scope");
            let mut outgoing = ServiceFs::new();
            let offer = ServiceOffer::<fexample::EchoService>::new_next()
                .add_named_next(&mut outgoing, "default", Service { scope: scope.to_handle() })
                .build_zircon_offer_next();
            context.serve_outgoing(&mut outgoing)?;
            scope.spawn(outgoing.collect());

            let node = context.take_node()?;
            let child_node = NodeBuilder::new("transport-child")
                .add_property("prop", "val")
                .add_offer(offer)
                .build();
            node.add_child(child_node).await?;

            info!("TestDriver started");
            Ok(Self { node, scope, tmp: Mutex::new("NA".to_string()) })
        }

        async fn stop(&self) {
            info!("TestDriver stopped. Tmp: '{}'", *self.tmp.lock().await);
        }
    }

    #[fuchsia::test]
    async fn test_basic() {
        let scope = fasync::Scope::new_with_name("test scope");
        let mut service_fs = ServiceFs::new();
        let offer = ServiceOffer::<fexample::EchoService>::new_next()
            .add_named_next(&mut service_fs, "default", Service { scope: scope.to_handle() })
            .build_zircon_offer_next();
        let mut harness = TestHarness::<TestDriver>::new()
            .set_driver_incoming(service_fs)
            .set_url("test_url")
            .add_offer(offer);

        let start_result = harness.start_driver().await;
        let started_driver = start_result.expect("success");
        let driver = started_driver.get_driver().expect("failed to get driver");
        driver.set_tmp("my_temp_var").await;
        assert_eq!("my_temp_var", driver.get_tmp().await);

        let service_proxy: ServiceInstance<fexample::EchoService> =
            started_driver.driver_outgoing().service().connect_next().unwrap();
        let (client_end, server_end) = fidl_next::fuchsia::create_channel();
        service_proxy.regular_echo(server_end).unwrap();
        let client = client_end.spawn();
        let resp = client.echo_string("echo to driver").await.unwrap();
        assert_eq!("resp", resp.response.as_str());
        started_driver.stop_driver().await;
    }

    #[fuchsia::test]
    async fn test_multiple_start_stop() {
        let scope = fasync::Scope::new_with_name("test scope");
        let mut service_fs = ServiceFs::new();
        let offer = ServiceOffer::<fexample::EchoService>::new_next()
            .add_named_next(&mut service_fs, "default", Service { scope: scope.to_handle() })
            .build_zircon_offer_next();
        let mut harness = TestHarness::<TestDriver>::new()
            .set_driver_incoming(service_fs)
            .set_url("test_url")
            .add_offer(offer);

        for i in 1..=3 {
            let start_result = harness.start_driver().await;
            let started_driver = start_result.expect("success");
            let driver = started_driver.get_driver().expect("failed to get driver");
            driver.set_tmp(format!("my_temp_var_{}", i).as_str()).await;
            assert_eq!(format!("my_temp_var_{}", i), driver.get_tmp().await);

            let service_proxy: ServiceInstance<fexample::EchoService> =
                started_driver.driver_outgoing().service().connect_next().unwrap();
            let (client_end, server_end) = fidl_next::fuchsia::create_channel();
            service_proxy.regular_echo(server_end).unwrap();
            let client = client_end.spawn();
            let resp = client.echo_string("echo to driver").await.unwrap();
            assert_eq!("resp", resp.response.as_str());
            started_driver.stop_driver().await;
        }
    }

    #[fuchsia::test]
    async fn test_no_start() {
        let _harness = TestHarness::<TestDriver>::default();
    }

    #[fuchsia::test]
    async fn test_start_fail() {
        let mut harness = TestHarness::<TestDriver>::new();
        let start_result = harness.start_driver().await;
        assert_eq!(start_result.err(), Some(Status::IO_REFUSED));
    }

    /// Records what the `DriverContext` start-argument accessors returned.
    struct StartArgsDriver {
        node_name: Option<String>,
        node_token: Option<zx::Event>,
        power_element_token: Option<zx::Event>,
        properties: Vec<(String, String, crate::PropertyValue)>,
    }

    impl Driver for StartArgsDriver {
        const NAME: &'static str = "start-args-driver";

        async fn start(mut context: crate::DriverContext) -> Result<Self, DriverError> {
            let properties = context
                .node_properties()
                .map(|p| (p.parent.to_string(), p.key.to_string(), p.value))
                .collect();
            let driver = Self {
                node_name: context.node_name().map(str::to_string),
                node_token: context.take_node_token(),
                power_element_token: context.take_power_element_token(),
                properties,
            };
            // Taking is one-shot.
            assert!(context.take_node_token().is_none());
            assert!(context.take_power_element_token().is_none());
            Ok(driver)
        }

        async fn stop(&self) {}
    }

    fn koid(handle: &impl zx::AsHandleRef) -> zx::Koid {
        handle.as_handle_ref().koid().unwrap()
    }

    #[fuchsia::test]
    async fn test_start_args_accessors() {
        let node_token = zx::Event::create();
        let power_token = zx::Event::create();
        let mut harness = TestHarness::<StartArgsDriver>::new()
            .set_node_name("my-node")
            .set_node_token(node_token.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
            .set_power_element_token(power_token.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
            .add_node_property("default", "fuchsia.BIND_PROTOCOL", 7u32)
            .add_node_property("default", "name", "value")
            .add_node_property("other-parent", "enabled", true)
            .add_node_property(
                "other-parent",
                "kind",
                crate::PropertyValue(fidl_fuchsia_driver_framework::NodePropertyValue::EnumValue(
                    "Kind.A".into(),
                )),
            );

        let started_driver = harness.start_driver().await.expect("driver starts");
        let driver = started_driver.get_driver().expect("driver");

        assert_eq!(driver.node_name.as_deref(), Some("my-node"));
        assert_eq!(driver.node_token.as_ref().map(koid), Some(koid(&node_token)));
        assert_eq!(driver.power_element_token.as_ref().map(koid), Some(koid(&power_token)));

        let as_strs: Vec<(&str, &str)> =
            driver.properties.iter().map(|(p, k, _)| (p.as_str(), k.as_str())).collect();
        assert_eq!(
            as_strs,
            [
                ("default", "fuchsia.BIND_PROTOCOL"),
                ("default", "name"),
                ("other-parent", "enabled"),
                ("other-parent", "kind"),
            ]
        );
        assert_eq!(driver.properties[0].2.as_int(), Some(7));
        assert_eq!(driver.properties[0].2.as_str(), None);
        assert_eq!(driver.properties[1].2.as_str(), Some("value"));
        assert_eq!(driver.properties[2].2.as_bool(), Some(true));
        assert_eq!(driver.properties[3].2.as_enum(), Some("Kind.A"));
        started_driver.stop_driver().await;
    }

    #[fuchsia::test]
    async fn test_start_args_accessors_absent() {
        let mut harness = TestHarness::<StartArgsDriver>::new();
        let started_driver = harness.start_driver().await.expect("driver starts");
        let driver = started_driver.get_driver().expect("driver");
        assert!(driver.node_name.is_none());
        assert!(driver.node_token.is_none());
        assert!(driver.power_element_token.is_none());
        assert!(driver.properties.is_empty());
        started_driver.stop_driver().await;
    }

    // Drivers that are still on the legacy bindings pass legacy power element args.
    #[fuchsia::test]
    async fn test_legacy_power_element_args() {
        let power_token = zx::Event::create();
        let mut harness = TestHarness::<StartArgsDriver>::new().set_power_element_args(
            fidl_fuchsia_driver_framework::PowerElementArgs {
                token: Some(power_token.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()),
                ..Default::default()
            },
        );
        let started_driver = harness.start_driver().await.expect("driver starts");
        let driver = started_driver.get_driver().expect("driver");
        assert_eq!(driver.power_element_token.as_ref().map(koid), Some(koid(&power_token)));
        started_driver.stop_driver().await;
    }
}
