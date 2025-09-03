// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::agent::AgentCreator;
use crate::base::SettingType;
use crate::handler::base::{Context, GenerateHandler};
use crate::handler::setting_handler::persist::ClientProxy;
use crate::handler::setting_handler::{BoxedController, ClientImpl};
use crate::ingress::fidl::Interface;
use crate::input::build_input_default_settings;
use crate::input::input_controller::InputController;
use crate::input::input_device_configuration::InputConfiguration;
use crate::input::types::InputInfoSources;
use crate::tests::fakes::camera3_service::Camera3Service;
use crate::tests::fakes::input_device_registry_service::InputDeviceRegistryService;
use crate::{
    AgentConfiguration, EnabledInterfacesConfiguration, Environment, EnvironmentBuilder,
    ServiceConfiguration, ServiceFlags,
};
use settings_common::config::default_settings::DefaultSetting;
use settings_common::config::AgentType;
use settings_common::inspect::config_logger::InspectConfigLogger;
use settings_test_common::fakes::service::ServiceRegistry;
use settings_test_common::storage::InMemoryStorageFactory;

use fidl_fuchsia_settings::{InputMarker, InputProxy};
use fuchsia_inspect::component;
use futures::channel::mpsc::UnboundedSender;
use futures::lock::Mutex;
use std::collections::HashSet;
use std::rc::Rc;

const ENV_NAME: &str = "settings_service_input_test_environment";

pub(crate) struct TestInputEnvironment {
    /// For sending requests to the input proxy.
    pub(crate) input_service: InputProxy,

    /// For sending media buttons changes.
    pub(crate) input_button_service: Rc<Mutex<InputDeviceRegistryService>>,

    /// For watching, connecting to, and making requests on the camera device.
    pub(crate) camera3_service: Rc<Mutex<Camera3Service>>,
}

pub(crate) struct TestInputEnvironmentBuilder {
    /// The initial InputInfoSources in the environment.
    starting_input_info_sources: Option<InputInfoSources>,

    /// The config to load from.
    input_device_config: Option<InputConfiguration>,

    /// The list of agents to include.
    agents: Vec<AgentType>,

    /// The list of additional media_button_event listeners.
    media_buttons_event_txs: Vec<UnboundedSender<settings_media_buttons::Event>>,
}

impl TestInputEnvironmentBuilder {
    pub(crate) fn new() -> Self {
        Self {
            starting_input_info_sources: None,
            input_device_config: None,
            agents: vec![AgentType::Restore, AgentType::MediaButtons],
            media_buttons_event_txs: vec![],
        }
    }

    pub(crate) fn set_starting_input_info_sources(
        mut self,
        starting_input_info_sources: InputInfoSources,
    ) -> Self {
        self.starting_input_info_sources = Some(starting_input_info_sources);
        self
    }

    pub(crate) fn set_input_device_config(
        mut self,
        input_device_config: InputConfiguration,
    ) -> Self {
        self.input_device_config = Some(input_device_config);
        self
    }

    pub(crate) fn add_media_buttons_event_tx(
        mut self,
        tx: UnboundedSender<settings_media_buttons::Event>,
    ) -> Self {
        self.media_buttons_event_txs.push(tx);
        self
    }

    pub(crate) async fn build(self) -> TestInputEnvironment {
        let service_registry = ServiceRegistry::create();
        let storage_factory = Rc::new(if let Some(info) = self.starting_input_info_sources {
            InMemoryStorageFactory::with_initial_data(&info)
        } else {
            InMemoryStorageFactory::new()
        });

        // Register fake input device registry service.
        let input_button_service_handle = Rc::new(Mutex::new(InputDeviceRegistryService::new()));
        service_registry.lock().await.register_service(input_button_service_handle.clone());

        // Register fake camera3 service.
        let camera3_service_handle = Rc::new(Mutex::new(Camera3Service::new(true)));
        service_registry.lock().await.register_service(camera3_service_handle.clone());

        let mut environment_builder = EnvironmentBuilder::new(Rc::clone(&storage_factory))
            // Need to add media buttons via configuration so channels get routed.
            .configuration(ServiceConfiguration::from(
                AgentConfiguration { agent_types: [AgentType::MediaButtons].into() },
                EnabledInterfacesConfiguration::with_interfaces(HashSet::new()),
                ServiceFlags::default(),
            ))
            .input_configuration(default_settings())
            .service(Box::new(ServiceRegistry::serve(service_registry)))
            // media buttons filtered from `from_type`.
            .agents(self.agents.into_iter().filter_map(AgentCreator::from_type).collect::<Vec<_>>())
            .media_buttons_event_txs(self.media_buttons_event_txs)
            .fidl_interfaces(&[Interface::Input]);

        if let Some(config) = self.input_device_config {
            // If hardware configuration was specified, we need a generate_handler to include the
            // specified configuration. This generate_handler method is a copy-paste of
            // persist::controller::spawn from setting_handler.rs, with the innermost controller
            // create method replaced with our custom constructor from input controller.
            let generate_handler: GenerateHandler = Box::new(move |context: Context| {
                let config = config.clone();
                let storage_factory = Rc::clone(&storage_factory);
                Box::pin(async move {
                    let setting_type = context.setting_type;
                    ClientImpl::create(
                        context,
                        Box::new(move |proxy| {
                            let config = config.clone();
                            let storage_factory = Rc::clone(&storage_factory);
                            Box::pin(async move {
                                let proxy = ClientProxy::new(proxy, setting_type).await;
                                let store = storage_factory.get_device_storage().await;
                                let controller_result = InputController::create_with_config(
                                    proxy,
                                    config.clone(),
                                    store,
                                );

                                controller_result.map(
                                    |controller: InputController<InMemoryStorageFactory>| {
                                        Box::new(controller) as BoxedController
                                    },
                                )
                            })
                        }),
                    )
                    .await
                })
            });

            environment_builder = environment_builder.handler(SettingType::Input, generate_handler);
        }

        let Environment { connector, .. } =
            environment_builder.spawn_nested(ENV_NAME).await.unwrap();

        let input_service = connector
            .expect("Nested environment should exist")
            .connect_to_protocol::<InputMarker>()
            .unwrap();

        TestInputEnvironment {
            input_service,
            input_button_service: input_button_service_handle,
            camera3_service: camera3_service_handle,
        }
    }
}

pub(super) fn default_settings() -> DefaultSetting<InputConfiguration, &'static str> {
    let config_logger =
        Rc::new(std::sync::Mutex::new(InspectConfigLogger::new(component::inspector().root())));
    build_input_default_settings(config_logger)
}
