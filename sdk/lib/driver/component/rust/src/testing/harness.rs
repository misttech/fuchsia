// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The harness provides a way to spin up drivers for unit testing.

use crate::testing::dut::DriverUnderTest;
use crate::testing::logsink_connector;
use crate::testing::node::NodeManager;
use crate::{Driver, Incoming};
use anyhow::Result;
use fdf::{AutoReleaseDispatcher, DispatcherBuilder, WeakDispatcher};
use fdf_env::Environment;
use fidl::endpoints::{ClientEnd, Proxy};
use fidl_fuchsia_driver_framework::Offer;
use fidl_next::{ClientEnd as NextClientEnd, CompatFrom, ServerEnd as NextServerEnd};
use fidl_next_fuchsia_component_runner::natural::ComponentNamespaceEntry;
use fidl_next_fuchsia_driver_framework::DriverStartArgs;
use fidl_next_fuchsia_driver_framework::natural::Offer as NextOffer;
use fuchsia_component::directory::open_directory_async;
use fuchsia_component::server::{ServiceFs, ServiceObj};
use futures::StreamExt;
use std::marker::PhantomData;
use std::sync::{Arc, Weak, mpsc};
use zx::{HandleBased, Status};
use {fidl_fuchsia_io as fio, fuchsia_async as fasync};

/// The main test harness for running a driver unit test.
pub struct TestHarness<D> {
    fdf_env_environment: Arc<Environment>,
    node_manager: Arc<NodeManager>,
    driver: Option<fdf_env::Driver<u32>>,
    dispatcher: AutoReleaseDispatcher,
    driver_incoming_dir: ClientEnd<fio::DirectoryMarker>,
    config_vmo: Option<zx::Vmo>,
    url: Option<String>,
    offers: Option<Vec<Offer>>,
    scope: fasync::Scope,
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
            AutoReleaseDispatcher::from(driver.new_dispatcher(dispatcher_builder).unwrap());
        let driver = Some(driver);

        Self {
            fdf_env_environment: env,
            node_manager,
            driver,
            dispatcher,
            driver_incoming_dir,
            config_vmo: None,
            url: None,
            offers: None,
            scope,
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

    /// Adds an offer to the driver's start args. Consumes and returns self to allow chaining.
    pub fn add_offer(mut self, offer: Offer) -> Self {
        self.offers.get_or_insert_default().push(offer);
        self
    }

    /// Gets a driver dispatcher that can be used to run test side driver transport client/servers.
    pub fn dispatcher(&self) -> WeakDispatcher {
        WeakDispatcher::from(&self.dispatcher)
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
            open_directory_async(&self.driver_incoming_dir, "svc", fio::R_STAR_DIR).unwrap();

        let start_args = DriverStartArgs {
            node: Some(NextClientEnd::from_untyped(node_client)),
            incoming: Some(vec![ComponentNamespaceEntry {
                path: Some("/svc".to_string()),
                directory: Some(NextClientEnd::from_untyped(
                    driver_incoming_svc.into_channel().unwrap().into(),
                )),
            }]),
            outgoing_dir: Some(NextServerEnd::compat_from(driver_outgoing_dir_server)),
            config: self
                .config_vmo
                .as_ref()
                .and_then(|v| v.duplicate_handle(fidl::Rights::SAME_RIGHTS).ok()),
            url: self.url.clone(),
            node_offers: self
                .offers
                .as_ref()
                .map(|o| o.clone().into_iter().map(NextOffer::compat_from).collect()),
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
            // SAFTEY: we created this through Box::into_raw below inside of new.
            let driver_value = unsafe { Box::from_raw(driver_ref.0 as *mut u32) };
            assert_eq!(*driver_value, 0x1234);
            shutdown_tx.send(()).unwrap();
        });

        shutdown_rx.recv().unwrap();

        self.fdf_env_environment.destroy_all_dispatchers();
        self.fdf_env_environment.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Node, NodeBuilder, ServiceInstance, ServiceOffer};
    use fidl_next::{Request, Responder};
    use fidl_next_fuchsia_examples::echo::{EchoString, SendString};
    use futures::StreamExt;
    use futures::lock::Mutex;
    use log::info;
    use {fidl_next_fuchsia_examples as fexample, fuchsia_async as fasync};

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
            server_end.spawn_on(EchoServer, &self.scope).detach_on_drop();
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

        async fn start(mut context: crate::DriverContext) -> Result<Self, Status> {
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
}
