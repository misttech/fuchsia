// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result, anyhow};
use fidl::endpoints::{ServerEnd, create_endpoints, create_proxy};
use fidl_fuchsia_component as fcomponent;
use fidl_fuchsia_component_decl as fdecl;
use fidl_fuchsia_power_broker as fbroker;
use fidl_fuchsia_power_topology_test as fpt;
use fidl_test_powerelementrunner::ControlMarker;
use fuchsia_async as fasync;
use fuchsia_component::client::{connect_to_protocol, connect_to_protocol_at_dir_root};
use fuchsia_component::server::ServiceFs;
use futures::StreamExt;
use log::{error, info, warn};
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use zx::Rights;

const ELEMENTS_COLLECTION: &'static str = "elements";

enum IncomingRequest {
    TopologyControl(fpt::TopologyControlRequestStream),
}

async fn lease(lessor: &fbroker::LessorProxy, level: u8) -> Result<fbroker::LeaseControlProxy> {
    let lease_control =
        lessor.lease(level).await?.map_err(|e| anyhow::anyhow!("{e:?}"))?.into_proxy();

    Ok(lease_control)
}

struct PowerElement {
    element_control: fbroker::ElementControlProxy,
    lessor: fbroker::LessorProxy,
    assertive_dependency_token: fbroker::DependencyToken,
    lease: RefCell<Option<fbroker::LeaseControlProxy>>,
    component_controller: fcomponent::ControllerProxy,
}

impl PowerElement {
    async fn new(
        realm: &fcomponent::RealmProxy,
        topology: &fbroker::TopologyProxy,
        element_name: &str,
        valid_levels: &[fbroker::PowerLevel],
        initial_current_level: fbroker::PowerLevel,
        dependencies: Vec<fbroker::LevelDependency>,
    ) -> Result<Self> {
        let (lessor_proxy, lessor_server_end) = create_proxy::<fbroker::LessorMarker>();
        let (element_control_proxy, element_control_server_end) =
            create_proxy::<fbroker::ElementControlMarker>();
        let (element_runner_client, element_runner_server) =
            create_endpoints::<fbroker::ElementRunnerMarker>();

        topology
            .add_element(fbroker::ElementSchema {
                element_name: Some(element_name.into()),
                initial_current_level: Some(initial_current_level),
                valid_levels: Some(valid_levels.to_vec()),
                dependencies: Some(dependencies),
                lessor_channel: Some(lessor_server_end),
                element_control: Some(element_control_server_end),
                element_runner: Some(element_runner_client),
                ..Default::default()
            })
            .await?
            .map_err(|d| anyhow!("{d:?}"))?;

        let assertive_dependency_token = fbroker::DependencyToken::create();
        element_control_proxy
            .register_dependency_token(
                assertive_dependency_token.duplicate_handle(Rights::SAME_RIGHTS)?,
            )
            .await?
            .map_err(|e| anyhow!("register assertive dependency token failed: {e:?}"))?;

        let (component_controller, controller_server_end) = create_proxy();
        let _ = realm
            .create_child(
                &fdecl::CollectionRef { name: ELEMENTS_COLLECTION.into() },
                &fdecl::Child {
                    name: Some(element_name.to_string()),
                    url: Some("#meta/power-element-runner.cm".to_string()),
                    startup: Some(fdecl::StartupMode::Eager),
                    ..Default::default()
                },
                fcomponent::CreateChildArgs {
                    controller: Some(controller_server_end),
                    ..Default::default()
                },
            )
            .await?;

        let (element_exposed_dir, element_exposed_dir_server) = create_proxy();
        realm
            .open_exposed_dir(
                &fdecl::ChildRef {
                    name: element_name.to_string(),
                    collection: Some(ELEMENTS_COLLECTION.to_string()),
                },
                element_exposed_dir_server,
            )
            .await
            .with_context(|| {
                format!("Failed to call open_exposed_dir for power element {}", element_name)
            })?
            .map_err(|e| {
                anyhow!("Failed to open exposed dir for power element {}: {:?}", element_name, e)
            })?;

        let proxy = connect_to_protocol_at_dir_root::<ControlMarker>(&element_exposed_dir)
            .with_context(|| {
                format!("Failed to connect to Control for power element {}", element_name)
            })?;
        proxy
            .start(element_name, initial_current_level, element_runner_server)
            .await
            .with_context(|| format!("Failed to call start for power element {}", element_name))?
            .map_err(|e| anyhow!("Failed to run power element {}: {:?}", element_name, e))?;

        Ok(Self {
            element_control: element_control_proxy,
            lessor: lessor_proxy,
            assertive_dependency_token,
            lease: RefCell::new(None),
            component_controller,
        })
    }
}

/// TopologyTestDaemon runs the server for test.power.topology FIDL APIs.
pub struct TopologyTestDaemon {
    topology_proxy: fbroker::TopologyProxy,
    // Holds elements and their leases for test.power.topology.TopologyControl.
    elements: RefCell<HashMap<String, PowerElement>>,
}

impl TopologyTestDaemon {
    pub async fn new() -> Result<Rc<Self>> {
        let topology_proxy = connect_to_protocol::<fbroker::TopologyMarker>()?;
        let elements = RefCell::new(HashMap::new());

        Ok(Rc::new(Self { topology_proxy, elements }))
    }

    pub async fn run(self: Rc<Self>) -> Result<()> {
        info!("Starting FIDL server");
        let mut service_fs = ServiceFs::new_local();

        service_fs.dir("svc").add_fidl_service(IncomingRequest::TopologyControl);
        service_fs
            .take_and_serve_directory_handle()
            .context("failed to serve outgoing namespace")?;

        let scope = fasync::Scope::new();
        let scope_handle = scope.to_handle();
        service_fs
            .for_each_concurrent(None, move |request: IncomingRequest| {
                let topology_proxy = self.topology_proxy.clone();
                let scope_handle = scope_handle.clone();
                async move {
                    match request {
                        IncomingRequest::TopologyControl(stream) => {
                            let ttd = Rc::new(TopologyTestDaemon {
                                topology_proxy,
                                elements: RefCell::new(HashMap::new()),
                            });
                            let _ = scope_handle
                                .spawn_local(ttd.handle_topology_control_request(stream));
                        }
                    }
                }
            })
            .await;
        scope.await;
        Ok(())
    }

    async fn handle_topology_control_request(
        self: Rc<Self>,
        mut stream: fpt::TopologyControlRequestStream,
    ) {
        while let Some(request) = stream.next().await {
            match request {
                Ok(fpt::TopologyControlRequest::Create { responder, elements }) => {
                    let result = responder.send(self.clone().create_topology(elements).await);

                    if let Err(error) = result {
                        warn!(error:?; "Error while responding to TopologyControl.Create request");
                    }
                }
                Ok(fpt::TopologyControlRequest::AcquireLease {
                    responder,
                    element_name,
                    level,
                    wait_for_status,
                }) => {
                    let result = responder.send(
                        self.clone().acquire_lease(element_name, level, wait_for_status).await,
                    );

                    if let Err(error) = result {
                        warn!(
                            error:?;
                            "Error while responding to TopologyControl.AcquireLease request"
                        );
                    }
                }
                Ok(fpt::TopologyControlRequest::DropLease { responder, element_name }) => {
                    let result = responder.send(self.clone().drop_lease(element_name).await);

                    if let Err(error) = result {
                        warn!(
                            error:?;
                            "Error while responding to TopologyControl.DropLease request"
                        );
                    }
                }
                Ok(
                    fidl_fuchsia_power_topology_test::TopologyControlRequest::OpenStatusChannel {
                        responder,
                        element_name,
                        status_channel,
                    },
                ) => {
                    let result = responder
                        .send(self.clone().open_status_channel(element_name, status_channel).await);
                    if let Err(error) = result {
                        warn!(
                            error:?;
                            "Error while responding to TopologyControl.OpenStatusChannel request"
                        );
                    }
                }
                Ok(fpt::TopologyControlRequest::_UnknownMethod { ordinal, .. }) => {
                    warn!(ordinal:?; "Unknown TopologyControl method");
                }
                Err(error) => {
                    error!(error:?; "Error handling TopologyControl request stream");
                }
            }
        }
    }

    async fn create_topology(
        self: Rc<Self>,
        mut elements: Vec<fpt::Element>,
    ) -> fpt::TopologyControlCreateResult {
        // Clear old topology when creating a new topology.
        for (_, element) in self.elements.borrow().iter() {
            let _ = element
                .component_controller
                .destroy()
                .await
                .expect("Failed to destroy old element instance");
        }
        self.elements.borrow_mut().clear();

        let realm = connect_to_protocol::<fcomponent::RealmMarker>().map_err(|err| {
            error!(err:%; "Failed to connect to fuchsia.component.Realm");
            fpt::CreateTopologyGraphError::Internal
        })?;

        {
            let mut internal_topology_elements = self.elements.borrow_mut();
            while elements.len() > 0 {
                let element = elements.pop().unwrap();
                self.clone()
                    .create_element_recursive(
                        &realm,
                        &mut internal_topology_elements,
                        element,
                        &mut elements,
                    )
                    .await?
            }
        }

        Ok(())
    }

    fn create_element_recursive<'a>(
        self: Rc<Self>,
        realm: &'a fcomponent::RealmProxy,
        internal_topology_elements: &'a mut HashMap<String, PowerElement>,
        element: fpt::Element,
        elements: &'a mut Vec<fpt::Element>,
    ) -> Pin<Box<dyn Future<Output = fpt::TopologyControlCreateResult> + 'a>> {
        Box::pin(async move {
            let mut dependencies = Vec::new();
            for dependency in element.dependencies {
                let required_element_name = dependency.requires_element;
                // If required_element hasn't been created, find it in `elements` and create it.
                if !internal_topology_elements.contains_key(&required_element_name) {
                    if let Some(index) =
                        elements.iter().position(|e| e.element_name == required_element_name)
                    {
                        let new_element = elements.swap_remove(index);
                        self.clone()
                            .create_element_recursive(
                                realm,
                                internal_topology_elements,
                                new_element,
                                elements,
                            )
                            .await?;
                    } else {
                        return Err(fpt::CreateTopologyGraphError::InvalidTopology);
                    }
                }
                let power_element = internal_topology_elements.get(&required_element_name).unwrap();
                let token = power_element
                    .assertive_dependency_token
                    .duplicate_handle(Rights::SAME_RIGHTS)
                    .expect("failed to duplicate token");
                dependencies.push(fbroker::LevelDependency {
                    dependent_level: Some(dependency.dependent_level),
                    requires_token: Some(token),
                    requires_level_by_preference: Some(vec![dependency.requires_level]),
                    ..Default::default()
                });
            }
            let element_name = element.element_name;
            let power_element = PowerElement::new(
                realm,
                &self.topology_proxy,
                &element_name,
                &element.valid_levels,
                element.initial_current_level,
                dependencies,
            )
            .await
            .map_err(|err| {
                error!(err:%, element_name:%; "Failed to create power element");
                fpt::CreateTopologyGraphError::Internal
            })?;

            internal_topology_elements.insert(element_name, power_element);
            Ok(())
        })
    }

    async fn acquire_lease(
        self: Rc<Self>,
        element_name: String,
        level: u8,
        wait_for_status: fbroker::LeaseStatus,
    ) -> fpt::TopologyControlAcquireLeaseResult {
        let elements = self.elements.borrow_mut();
        let element = elements.get(&element_name).ok_or_else(|| {
            warn!(element_name:%; "Failed to find element name in the created topology graph");
            fpt::LeaseControlError::InvalidElement
        })?;

        let lease_control = lease(&element.lessor, level).await.map_err(|err| {
            warn!(err:%, element_name:%, level; "Failed to acquire a lease");
            fpt::LeaseControlError::Internal
        })?;

        if wait_for_status != fbroker::LeaseStatus::Unknown {
            let target_status = wait_for_status;
            let mut current_status = fbroker::LeaseStatus::Unknown;
            loop {
                current_status =
                    lease_control.watch_status(current_status).await.map_err(|err| {
                        warn!(err:%, element_name:%, level; "Failed to watch lease status");
                        fpt::LeaseControlError::Internal
                    })?;
                if current_status == target_status {
                    break;
                }
            }
        }

        let _ = element.lease.borrow_mut().replace(lease_control);

        Ok(())
    }

    async fn drop_lease(
        self: Rc<Self>,
        element_name: String,
    ) -> fpt::TopologyControlDropLeaseResult {
        let elements = self.elements.borrow();
        let element = elements.get(&element_name).ok_or_else(|| {
            warn!(element_name:%; "Failed to find element name in the created topology graph");
            fpt::LeaseControlError::InvalidElement
        })?;
        element.lease.borrow_mut().take();

        Ok(())
    }

    async fn open_status_channel(
        self: Rc<Self>,
        element_name: String,
        status_channel: ServerEnd<fbroker::StatusMarker>,
    ) -> fpt::TopologyControlOpenStatusChannelResult {
        let elements = self.elements.borrow_mut();
        let element = elements.get(&element_name).ok_or_else(|| {
            warn!(element_name:%; "Failed to find element name in the created topology graph");
            fpt::OpenStatusChannelError::InvalidElement
        })?;

        let _ = element.element_control.open_status_channel(status_channel).map_err(|err| {
            warn!(err:%, element_name:%; "Failed to open_status_channel");
            fpt::OpenStatusChannelError::Internal
        })?;

        Ok(())
    }
}
