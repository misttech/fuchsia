// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::AtomicU64;

#[cfg(test)]
use anyhow::format_err;
use anyhow::{Context, Error};
use audio::AudioInfoLoader;
use audio::types::AudioInfo;
use display::display_controller::DisplayInfoLoader;
use fidl_fuchsia_io::DirectoryProxy;
use fidl_fuchsia_settings::{
    IntlRequestStream, KeyboardRequestStream, LightRequestStream, NightModeRequestStream,
    PrivacyRequestStream, SetupRequestStream,
};
use fidl_fuchsia_stash::StoreProxy;
use fuchsia_component::client::connect_to_protocol;
#[cfg(test)]
use fuchsia_component::server::ProtocolConnector;
use fuchsia_component::server::{ServiceFs, ServiceFsDir, ServiceObjLocal};
use fuchsia_inspect::component;
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::lock::Mutex;
use futures::{StreamExt, TryStreamExt};
#[cfg(test)]
use log as _;
use settings_common::config::default_settings::DefaultSetting;
use settings_common::config::{AgentType, ControllerFlag};
use settings_common::inspect::event::{
    ExternalEventPublisher, SettingValuePublisher, UsageEvent, UsagePublisher,
};
use settings_common::inspect::listener_logger::ListenerInspectLogger;
use settings_common::service_context::{
    ExternalServiceEvent, GenerateService, ServiceContext as CommonServiceContext,
};
use settings_light::light_controller::LightController;
use settings_storage::device_storage::DeviceStorage;
use settings_storage::fidl_storage::FidlStorage;
use settings_storage::storage_factory::{FidlStorageFactory, StorageFactory};
use zx::MonotonicDuration;
use {fidl_fuchsia_update_verify as fupdate, fuchsia_async as fasync};

pub use display::display_configuration::DisplayConfiguration;
pub use handler::setting_proxy_inspect_info::SettingProxyInspectInfo;
pub use input::input_device_configuration::InputConfiguration;
use serde::Deserialize;
pub use service::{Address, Payload};
pub use settings_light::light_hardware_configuration::LightHardwareConfiguration;

use crate::accessibility::accessibility_controller::AccessibilityController;
use crate::agent::authority::Authority;
use crate::agent::{AgentCreator, Lifespan};
use crate::audio::audio_controller::AudioController;
use crate::base::{Dependency, Entity, SettingType};
use crate::display::display_controller::{DisplayController, ExternalBrightnessControl};
use crate::do_not_disturb::do_not_disturb_controller::DoNotDisturbController;
use crate::factory_reset::factory_reset_controller::FactoryResetController;
use crate::handler::base::GenerateHandler;
use crate::handler::setting_handler::persist::Handler as DataHandler;
use crate::handler::setting_handler_factory_impl::SettingHandlerFactoryImpl;
use crate::handler::setting_proxy::SettingProxy;
use crate::ingress::fidl;
use crate::ingress::registration::Registrant;
use crate::input::input_controller::InputController;
use crate::intl::intl_controller::IntlController;
use crate::job::manager::Manager;
use crate::job::source::Seeder;
use crate::keyboard::keyboard_controller::KeyboardController;
use crate::night_mode::night_mode_controller::NightModeController;
use crate::privacy::privacy_controller::PrivacyController;
use crate::service::message::Delegate;
use crate::service_context::ServiceContext;
use crate::setup::setup_controller::SetupController;

mod accessibility;
pub mod audio;
mod clock;
pub mod display;
mod do_not_disturb;
mod event;
mod factory_reset;
pub mod input;
mod intl;
mod job;
mod keyboard;
mod night_mode;
mod privacy;
mod service;
mod setup;
mod storage_migrations;

pub mod agent;
pub mod base;
pub mod config;
pub mod handler;
pub mod ingress;
pub mod inspect;
pub mod message;
pub(crate) mod migration;
pub mod service_context;
pub mod storage;
pub mod trace;

/// This value represents the duration the proxy will wait after the last request
/// before initiating the teardown of the controller. If a request is received
/// before the timeout triggers, then the timeout will be canceled.
// The value of 5 seconds was chosen arbitrarily to allow some time between manual
// button presses that occurs for some settings.
pub(crate) const DEFAULT_TEARDOWN_TIMEOUT: MonotonicDuration = MonotonicDuration::from_seconds(5);
const DEFAULT_SETTING_PROXY_MAX_ATTEMPTS: u64 = 3;
const DEFAULT_SETTING_PROXY_RESPONSE_TIMEOUT_MS: i64 = 10_000;

/// A common trigger for exiting.
pub type ExitSender = futures::channel::mpsc::UnboundedSender<()>;

/// Runtime defines where the environment will exist. Service is meant for
/// production environments and will hydrate components to be discoverable as
/// an environment service. Nested creates a service only usable in the scope
/// of a test.
#[derive(PartialEq)]
enum Runtime {
    Service,
    #[cfg(test)]
    Nested(&'static str),
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct AgentConfiguration {
    pub agent_types: HashSet<AgentType>,
}

#[derive(PartialEq, Debug, Clone, Deserialize)]
pub struct EnabledInterfacesConfiguration {
    pub interfaces: HashSet<fidl::InterfaceSpec>,
}

impl EnabledInterfacesConfiguration {
    pub fn with_interfaces(interfaces: HashSet<fidl::InterfaceSpec>) -> Self {
        Self { interfaces }
    }
}

#[derive(Default, Debug, Clone, Deserialize)]
pub struct ServiceFlags {
    pub controller_flags: HashSet<ControllerFlag>,
}

#[derive(PartialEq, Debug, Default, Clone)]
pub struct ServiceConfiguration {
    agent_types: HashSet<AgentType>,
    fidl_interfaces: HashSet<fidl::Interface>,
    controller_flags: HashSet<ControllerFlag>,
}

impl ServiceConfiguration {
    pub fn from(
        agent_types: AgentConfiguration,
        interfaces: EnabledInterfacesConfiguration,
        flags: ServiceFlags,
    ) -> Self {
        let fidl_interfaces: HashSet<fidl::Interface> =
            interfaces.interfaces.into_iter().map(|x| x.into()).collect();

        Self {
            agent_types: agent_types.agent_types,
            fidl_interfaces,
            controller_flags: flags.controller_flags,
        }
    }

    fn set_fidl_interfaces(&mut self, interfaces: HashSet<fidl::Interface>) {
        self.fidl_interfaces = interfaces;
    }

    fn set_controller_flags(&mut self, controller_flags: HashSet<ControllerFlag>) {
        self.controller_flags = controller_flags;
    }
}

/// Environment is handed back when an environment is spawned from the
/// EnvironmentBuilder. A nested environment (if available) is returned,
/// along with a receiver to be notified when initialization/setup is
/// complete.
#[cfg(test)]
pub struct Environment {
    pub connector: Option<ProtocolConnector>,
    pub delegate: Delegate,
    pub entities: HashSet<Entity>,
    pub job_seeder: Seeder,
}

#[cfg(test)]
impl Environment {
    pub fn new(
        connector: Option<ProtocolConnector>,
        delegate: Delegate,
        job_seeder: Seeder,
        entities: HashSet<Entity>,
    ) -> Environment {
        Environment { connector, delegate, job_seeder, entities }
    }
}

#[cfg(test)]
fn init_storage_dir() -> DirectoryProxy {
    let tempdir = tempfile::tempdir().expect("failed to create tempdir");
    fuchsia_fs::directory::open_in_namespace(
        tempdir.path().to_str().expect("tempdir path is not valid UTF-8"),
        fuchsia_fs::PERM_READABLE | fuchsia_fs::PERM_WRITABLE,
    )
    .expect("failed to open connection to tempdir")
}

#[cfg(not(test))]
fn init_storage_dir() -> DirectoryProxy {
    panic!("migration dir must be specified");
}

/// The [EnvironmentBuilder] aggregates the parameters surrounding an [environment](Environment) and
/// ultimately spawns an environment based on them.
pub struct EnvironmentBuilder<'a, T: StorageFactory<Storage = DeviceStorage>> {
    configuration: Option<ServiceConfiguration>,
    agent_blueprints: Vec<AgentCreator>,
    event_subscriber_blueprints: Vec<event::subscriber::BlueprintHandle>,
    storage_factory: Rc<T>,
    generate_service: Option<GenerateService>,
    registrants: Vec<Registrant>,
    settings: Vec<SettingType>,
    handlers: HashMap<SettingType, GenerateHandler>,
    setting_proxy_inspect_info: Option<&'a fuchsia_inspect::Node>,
    active_listener_inspect_logger: Option<Rc<ListenerInspectLogger>>,
    storage_dir: Option<DirectoryProxy>,
    store_proxy: Option<StoreProxy>,
    fidl_storage_factory: Option<Rc<FidlStorageFactory>>,
    display_configuration: Option<DefaultSetting<DisplayConfiguration, &'static str>>,
    audio_configuration: Option<DefaultSetting<AudioInfo, &'static str>>,
    input_configuration: Option<DefaultSetting<InputConfiguration, &'static str>>,
    light_configuration: Option<DefaultSetting<LightHardwareConfiguration, &'static str>>,
    media_buttons_event_txs: Vec<UnboundedSender<settings_media_buttons::Event>>,
}

impl<'a, T: StorageFactory<Storage = DeviceStorage> + 'static> EnvironmentBuilder<'a, T> {
    /// Construct a new [EnvironmentBuilder] using `storage_factory` to construct the storage for
    /// the future [Environment].
    pub fn new(storage_factory: Rc<T>) -> Self {
        EnvironmentBuilder {
            configuration: None,
            agent_blueprints: vec![],
            event_subscriber_blueprints: vec![],
            storage_factory,
            generate_service: None,
            handlers: HashMap::new(),
            registrants: vec![],
            settings: vec![],
            setting_proxy_inspect_info: None,
            active_listener_inspect_logger: None,
            storage_dir: None,
            store_proxy: None,
            fidl_storage_factory: None,
            display_configuration: None,
            audio_configuration: None,
            input_configuration: None,
            light_configuration: None,
            media_buttons_event_txs: vec![],
        }
    }

    /// Overrides the default [GenerateHandler] for a specific [SettingType].
    pub fn handler(mut self, setting_type: SettingType, generate_handler: GenerateHandler) -> Self {
        // Ignore the old handler result.
        let _ = self.handlers.insert(setting_type, generate_handler);
        self
    }

    /// A service generator to be used as an overlay on the ServiceContext.
    pub fn service(mut self, generate_service: GenerateService) -> Self {
        self.generate_service = Some(generate_service);
        self
    }

    /// A preset configuration to load preset parameters as a base. Note that this will override
    /// any configuration modifications made by [EnvironmentBuilder::fidl_interface],
    /// [EnvironmentBuilder::policies], and [EnvironmentBuilder::flags].
    pub fn configuration(mut self, configuration: ServiceConfiguration) -> Self {
        self.configuration = Some(configuration);
        self
    }

    pub fn display_configuration(
        mut self,
        display_configuration: DefaultSetting<DisplayConfiguration, &'static str>,
    ) -> Self {
        self.display_configuration = Some(display_configuration);
        self
    }

    pub fn audio_configuration(
        mut self,
        audio_configuration: DefaultSetting<AudioInfo, &'static str>,
    ) -> Self {
        self.audio_configuration = Some(audio_configuration);
        self
    }

    pub fn input_configuration(
        mut self,
        input_configuration: DefaultSetting<InputConfiguration, &'static str>,
    ) -> Self {
        self.input_configuration = Some(input_configuration);
        self
    }

    pub fn light_configuration(
        mut self,
        light_configuration: DefaultSetting<LightHardwareConfiguration, &'static str>,
    ) -> Self {
        self.light_configuration = Some(light_configuration);
        self
    }

    /// Will override all fidl interfaces in the [ServiceConfiguration].
    pub fn fidl_interfaces(mut self, interfaces: &[fidl::Interface]) -> Self {
        if self.configuration.is_none() {
            self.configuration = Some(ServiceConfiguration::default());
        }

        if let Some(c) = self.configuration.as_mut() {
            c.set_fidl_interfaces(interfaces.iter().copied().collect());
        }

        self
    }

    /// Appends the [Registrant]s to the list of registrants already configured.
    pub fn registrants(mut self, mut registrants: Vec<Registrant>) -> Self {
        self.registrants.append(&mut registrants);

        self
    }

    /// Setting types to participate.
    pub fn settings(mut self, settings: &[SettingType]) -> Self {
        self.settings.extend(settings);

        self
    }

    /// Setting types to participate with customized controllers.
    pub fn flags(mut self, controller_flags: &[ControllerFlag]) -> Self {
        if self.configuration.is_none() {
            self.configuration = Some(ServiceConfiguration::default());
        }

        if let Some(c) = self.configuration.as_mut() {
            c.set_controller_flags(controller_flags.iter().copied().collect());
        }

        self
    }

    /// Appends the supplied [AgentRegistrar]s to the list of agent registrars.
    pub fn agents(mut self, mut registrars: Vec<AgentCreator>) -> Self {
        self.agent_blueprints.append(&mut registrars);
        self
    }

    /// Event subscribers to participate
    pub fn event_subscribers(mut self, subscribers: &[event::subscriber::BlueprintHandle]) -> Self {
        self.event_subscriber_blueprints.append(&mut subscribers.to_vec());
        self
    }

    /// Sets the inspect node for setting proxy inspect information and any required
    /// inspect loggers.
    pub fn setting_proxy_inspect_info(
        mut self,
        setting_proxy_inspect_info: &'a fuchsia_inspect::Node,
        active_listener_inspect_logger: Rc<ListenerInspectLogger>,
    ) -> Self {
        self.setting_proxy_inspect_info = Some(setting_proxy_inspect_info);
        self.active_listener_inspect_logger = Some(active_listener_inspect_logger);
        self
    }

    pub fn storage_dir(mut self, storage_dir: DirectoryProxy) -> Self {
        self.storage_dir = Some(storage_dir);
        self
    }

    pub fn store_proxy(mut self, store_proxy: StoreProxy) -> Self {
        self.store_proxy = Some(store_proxy);
        self
    }

    pub fn fidl_storage_factory(mut self, fidl_storage_factory: Rc<FidlStorageFactory>) -> Self {
        self.fidl_storage_factory = Some(fidl_storage_factory);
        self
    }

    pub fn media_buttons_event_txs(
        mut self,
        media_buttons_event_txs: Vec<UnboundedSender<settings_media_buttons::Event>>,
    ) -> Self {
        self.media_buttons_event_txs.extend(media_buttons_event_txs);
        self
    }

    /// Prepares an environment so that it may be spawned. This ensures that all necessary
    /// components are spawned and ready to handle events and FIDL requests.
    async fn prepare_env(
        mut self,
        mut fs: ServiceFs<ServiceObjLocal<'_, ()>>,
        runtime: Runtime,
    ) -> Result<(ServiceFs<ServiceObjLocal<'_, ()>>, Delegate, Seeder, HashSet<Entity>), Error>
    {
        let mut service_dir = match runtime {
            Runtime::Service => fs.dir("svc"),
            #[cfg(test)]
            Runtime::Nested(_) => fs.root_dir(),
        };

        let _ = service_dir.add_fidl_service(
            move |mut stream: fupdate::ComponentOtaHealthCheckRequestStream| {
                fasync::Task::local(async move {
                    while let Some(fupdate::ComponentOtaHealthCheckRequest::GetHealthStatus {
                        responder,
                    }) = stream.try_next().await.expect("error running health check service")
                    {
                        // We always respond healthy because the health check can only be served
                        // if the environment is able to spawn which in turn guarantees that no agents
                        // have returned an error.
                        responder
                            .send(fupdate::HealthStatus::Healthy)
                            .expect("failed to send health status");
                    }
                })
                .detach();
            },
        );

        // Define top level MessageHub for service communication.
        let delegate = service::MessageHub::create_hub();

        let (agent_types, fidl_interfaces, flags) = match self.configuration {
            Some(configuration) => (
                configuration.agent_types,
                configuration.fidl_interfaces,
                configuration.controller_flags,
            ),
            _ => (HashSet::new(), HashSet::new(), HashSet::new()),
        };

        self.registrants.extend(fidl_interfaces.into_iter().map(|x| x.registrant()));

        let mut settings = HashSet::new();
        settings.extend(self.settings);

        for registrant in &self.registrants {
            for dependency in registrant.get_dependencies() {
                match dependency {
                    Dependency::Entity(Entity::Handler(setting_type)) => {
                        let _ = settings.insert(*setting_type);
                    }
                }
            }
        }

        let fidl_storage_factory = if let Some(factory) = self.fidl_storage_factory {
            factory
        } else {
            let (migration_id, storage_dir) = if let Some(storage_dir) = self.storage_dir {
                let store_proxy = self.store_proxy.unwrap_or_else(|| {
                    let store_proxy = connect_to_protocol::<fidl_fuchsia_stash::StoreMarker>()
                        .expect("failed to connect to stash");
                    store_proxy
                        .identify("setting_service")
                        .expect("should be able to identify to stash");
                    store_proxy
                });

                let migration_manager = storage_migrations::register_migrations(
                    &settings,
                    Clone::clone(&storage_dir),
                    store_proxy,
                )
                .context("failed to register migrations")?;
                let migration_id = match migration_manager.run_migrations().await {
                    Ok(id) => {
                        log::info!("migrated storage to {id:?}");
                        id
                    }
                    Err((id, e)) => {
                        log::error!("Settings migration failed: {e:?}");
                        id
                    }
                };
                let migration_id = migration_id.map(|migration| migration.migration_id);
                (migration_id, storage_dir)
            } else {
                (None, init_storage_dir())
            };

            Rc::new(FidlStorageFactory::new(migration_id.unwrap_or(0), storage_dir))
        };

        let common_service_context = Rc::new(CommonServiceContext::new(self.generate_service));
        let service_context = Rc::new(ServiceContext::new_from_common(
            Rc::clone(&common_service_context),
            Some(delegate.clone()),
        ));

        let context_id_counter = Rc::new(AtomicU64::new(1));

        let mut handler_factory = SettingHandlerFactoryImpl::new(
            settings.clone(),
            Rc::clone(&service_context),
            context_id_counter.clone(),
        );

        Self::initialize_storage(&settings, &*fidl_storage_factory, &*self.storage_factory).await;

        EnvironmentBuilder::register_setting_handlers(
            &settings,
            Rc::clone(&self.storage_factory),
            &flags,
            self.display_configuration.map(DisplayInfoLoader::new),
            self.audio_configuration.map(AudioInfoLoader::new),
            self.input_configuration,
            &mut handler_factory,
        )
        .await;

        let listener_logger = self
            .active_listener_inspect_logger
            .unwrap_or_else(|| Rc::new(ListenerInspectLogger::new()));

        let RegistrationResult {
            media_buttons_event_txs,
            setting_value_rx,
            external_event_rx,
            usage_event_rx,
            tasks,
        } = Self::register_controllers(
            &settings,
            Rc::clone(&common_service_context),
            fidl_storage_factory,
            self.storage_factory,
            self.light_configuration,
            &mut service_dir,
            Rc::clone(&listener_logger),
        )
        .await;
        for task in tasks {
            task.detach();
        }

        self.media_buttons_event_txs.extend(media_buttons_event_txs);

        // Override the configuration handlers with any custom handlers specified
        // in the environment.
        for (setting_type, handler) in self.handlers {
            handler_factory.register(setting_type, handler);
        }

        let agent_blueprints = create_agent_blueprints(
            agent_types,
            self.agent_blueprints,
            self.media_buttons_event_txs,
            setting_value_rx,
            external_event_rx,
            usage_event_rx,
        );

        let job_manager_signature = Manager::spawn(&delegate).await;
        let job_seeder = Seeder::new(&delegate, job_manager_signature).await;

        let entities = create_environment(
            service_dir,
            delegate.clone(),
            job_seeder.clone(),
            settings,
            self.registrants,
            agent_blueprints,
            self.event_subscriber_blueprints,
            service_context,
            Rc::new(Mutex::new(handler_factory)),
            self.setting_proxy_inspect_info.unwrap_or_else(|| component::inspector().root()),
            listener_logger,
        )
        .await
        .context("Could not create environment")?;

        Ok((fs, delegate, job_seeder, entities))
    }

    /// Spawn an [Environment] on the supplied [fasync::LocalExecutor] so that it may process
    /// incoming FIDL requests.
    pub fn spawn(
        self,
        mut executor: fasync::LocalExecutor,
        fs: ServiceFs<ServiceObjLocal<'_, ()>>,
    ) -> Result<(), Error> {
        let (mut fs, ..) = executor
            .run_singlethreaded(self.prepare_env(fs, Runtime::Service))
            .context("Failed to prepare env")?;

        let _ = fs.take_and_serve_directory_handle().expect("could not service directory handle");
        executor.run_singlethreaded(fs.collect::<()>());
        Ok(())
    }

    /// Spawn a nested [Environment] so that it can be used for tests.
    #[cfg(test)]
    pub async fn spawn_nested(self, env_name: &'static str) -> Result<Environment, Error> {
        let (mut fs, delegate, job_seeder, entities) = self
            .prepare_env(ServiceFs::new_local(), Runtime::Nested(env_name))
            .await
            .context("Failed to prepare env")?;
        let connector = Some(fs.create_protocol_connector()?);
        fasync::Task::local(fs.collect()).detach();

        Ok(Environment::new(connector, delegate, job_seeder, entities))
    }

    /// Spawns a nested environment and returns the associated
    /// ProtocolConnector. Note that this is a helper function that provides a
    /// shortcut for calling EnvironmentBuilder::name() and
    /// EnvironmentBuilder::spawn().
    #[cfg(test)]
    pub async fn spawn_and_get_protocol_connector(
        self,
        env_name: &'static str,
    ) -> Result<ProtocolConnector, Error> {
        let environment = self.spawn_nested(env_name).await?;

        environment.connector.ok_or_else(|| format_err!("connector not created"))
    }
}

struct RegistrationResult {
    media_buttons_event_txs: Vec<UnboundedSender<settings_media_buttons::Event>>,
    setting_value_rx: UnboundedReceiver<(&'static str, String)>,
    external_event_rx: UnboundedReceiver<ExternalServiceEvent>,
    usage_event_rx: UnboundedReceiver<UsageEvent>,
    tasks: Vec<fasync::Task<()>>,
}

impl<'a, T: StorageFactory<Storage = DeviceStorage> + 'static> EnvironmentBuilder<'a, T> {
    async fn initialize_storage<F, D>(
        components: &HashSet<SettingType>,
        fidl_storage_factory: &F,
        device_storage_factory: &D,
    ) where
        F: StorageFactory<Storage = FidlStorage>,
        D: StorageFactory<Storage = DeviceStorage>,
    {
        if components.contains(&SettingType::Intl) {
            device_storage_factory
                .initialize::<IntlController>()
                .await
                .expect("storage should still be initializing");
        }

        if components.contains(&SettingType::Keyboard) {
            device_storage_factory
                .initialize::<KeyboardController>()
                .await
                .expect("storage should still be initializing");
        }

        if components.contains(&SettingType::Light) {
            fidl_storage_factory
                .initialize::<LightController>()
                .await
                .expect("storage should still be initializing");
        }

        if components.contains(&SettingType::NightMode) {
            device_storage_factory
                .initialize::<NightModeController>()
                .await
                .expect("storage should still be initializing");
        }

        if components.contains(&SettingType::Privacy) {
            device_storage_factory
                .initialize::<PrivacyController>()
                .await
                .expect("storage should still be initializing");
        }

        if components.contains(&SettingType::Setup) {
            device_storage_factory
                .initialize::<SetupController>()
                .await
                .expect("storage should still be initializing");
        }
    }

    async fn register_controllers<F, D>(
        components: &HashSet<SettingType>,
        service_context: Rc<CommonServiceContext>,
        fidl_storage_factory: Rc<F>,
        device_storage_factory: Rc<D>,
        light_configuration: Option<DefaultSetting<LightHardwareConfiguration, &'static str>>,
        service_dir: &mut ServiceFsDir<'_, ServiceObjLocal<'_, ()>>,
        listener_logger: Rc<ListenerInspectLogger>,
    ) -> RegistrationResult
    where
        F: StorageFactory<Storage = FidlStorage>,
        D: StorageFactory<Storage = DeviceStorage>,
    {
        let (setting_value_tx, setting_value_rx) = mpsc::unbounded();
        let (external_event_tx, external_event_rx) = mpsc::unbounded();
        let (usage_event_tx, usage_event_rx) = mpsc::unbounded();
        let external_publisher = ExternalEventPublisher::new(external_event_tx);
        let mut media_buttons_event_txs = vec![];
        let mut tasks = vec![];

        // Start handlers for all components.
        if components.contains(&SettingType::Light) {
            let mut light_configuration =
                light_configuration.expect("Light controller requires a light configuration");
            match settings_light::setup_light_api(
                Rc::clone(&service_context),
                &mut light_configuration,
                fidl_storage_factory,
                SettingValuePublisher::new(setting_value_tx.clone()),
                UsagePublisher::new(usage_event_tx.clone(), Rc::clone(&listener_logger)),
                external_publisher.clone(),
            )
            .await
            {
                Ok(settings_light::SetupResult {
                    mut light_fidl_handler,
                    media_buttons_event_tx,
                    task,
                }) => {
                    media_buttons_event_txs.push(media_buttons_event_tx);
                    tasks.push(task);
                    let _ = service_dir.add_fidl_service(move |stream: LightRequestStream| {
                        light_fidl_handler.handle_stream(stream)
                    });
                }
                Err(e) => {
                    log::error!("Failed to setup light api: {e:?}");
                }
            }
        }

        if components.contains(&SettingType::Intl) {
            let intl::SetupResult { mut intl_fidl_handler, task } = intl::setup_intl_api(
                Rc::clone(&device_storage_factory),
                SettingValuePublisher::new(setting_value_tx.clone()),
                UsagePublisher::new(usage_event_tx.clone(), Rc::clone(&listener_logger)),
            )
            .await;
            tasks.push(task);
            let _ = service_dir.add_fidl_service(move |stream: IntlRequestStream| {
                intl_fidl_handler.handle_stream(stream)
            });
        }

        if components.contains(&SettingType::Keyboard) {
            let keyboard::SetupResult { mut keyboard_fidl_handler, task } =
                keyboard::setup_keyboard_api(
                    Rc::clone(&device_storage_factory),
                    SettingValuePublisher::new(setting_value_tx.clone()),
                    UsagePublisher::new(usage_event_tx.clone(), Rc::clone(&listener_logger)),
                )
                .await;
            tasks.push(task);
            let _ = service_dir.add_fidl_service(move |stream: KeyboardRequestStream| {
                keyboard_fidl_handler.handle_stream(stream)
            });
        }

        if components.contains(&SettingType::NightMode) {
            let night_mode::SetupResult { mut night_mode_fidl_handler, task } =
                night_mode::setup_night_mode_api(
                    Rc::clone(&device_storage_factory),
                    SettingValuePublisher::new(setting_value_tx.clone()),
                    UsagePublisher::new(usage_event_tx.clone(), Rc::clone(&listener_logger)),
                )
                .await;
            tasks.push(task);
            let _ = service_dir.add_fidl_service(move |stream: NightModeRequestStream| {
                night_mode_fidl_handler.handle_stream(stream)
            });
        }

        if components.contains(&SettingType::Privacy) {
            let privacy::SetupResult { mut privacy_fidl_handler, task } =
                privacy::setup_privacy_api(
                    Rc::clone(&device_storage_factory),
                    SettingValuePublisher::new(setting_value_tx.clone()),
                    UsagePublisher::new(usage_event_tx.clone(), Rc::clone(&listener_logger)),
                )
                .await;
            tasks.push(task);
            let _ = service_dir.add_fidl_service(move |stream: PrivacyRequestStream| {
                privacy_fidl_handler.handle_stream(stream)
            });
        }

        if components.contains(&SettingType::Setup) {
            let setup::SetupResult { mut setup_fidl_handler, task } = setup::setup_setup_api(
                service_context,
                device_storage_factory,
                SettingValuePublisher::new(setting_value_tx),
                UsagePublisher::new(usage_event_tx, listener_logger),
                external_publisher,
            )
            .await;
            tasks.push(task);
            let _ = service_dir.add_fidl_service(move |stream: SetupRequestStream| {
                setup_fidl_handler.handle_stream(stream)
            });
        }

        RegistrationResult {
            media_buttons_event_txs,
            setting_value_rx,
            external_event_rx,
            usage_event_rx,
            tasks,
        }
    }

    /// Initializes storage and registers handler generation functions for the configured setting
    /// types.
    async fn register_setting_handlers(
        components: &HashSet<SettingType>,
        device_storage_factory: Rc<T>,
        controller_flags: &HashSet<ControllerFlag>,
        display_loader: Option<DisplayInfoLoader>,
        audio_loader: Option<AudioInfoLoader>,
        input_configuration: Option<DefaultSetting<InputConfiguration, &'static str>>,
        factory_handle: &mut SettingHandlerFactoryImpl,
    ) {
        // Accessibility
        if components.contains(&SettingType::Accessibility) {
            device_storage_factory
                .initialize::<AccessibilityController<T>>()
                .await
                .expect("storage should still be initializing");
            let device_storage_factory = Rc::clone(&device_storage_factory);
            factory_handle.register(
                SettingType::Accessibility,
                Box::new(move |context| {
                    DataHandler::<AccessibilityController<T>>::spawn_with_async(
                        context,
                        Rc::clone(&device_storage_factory),
                    )
                }),
            );
        }

        // Audio
        if components.contains(&SettingType::Audio) {
            let audio_loader = audio_loader.expect("Audio storage requires audio loader");
            device_storage_factory
                .initialize_with_loader::<AudioController<T>, _>(audio_loader.clone())
                .await
                .expect("storage should still be initializing");
            let device_storage_factory = Rc::clone(&device_storage_factory);
            factory_handle.register(
                SettingType::Audio,
                Box::new(move |context| {
                    DataHandler::<AudioController<T>>::spawn_with_async(
                        context,
                        (Rc::clone(&device_storage_factory), audio_loader.clone()),
                    )
                }),
            );
        }

        // Display
        if components.contains(&SettingType::Display) {
            device_storage_factory
                .initialize_with_loader::<DisplayController<T>, _>(
                    display_loader.expect("Display storage requires display loader"),
                )
                .await
                .expect("storage should still be initializing");
            let device_storage_factory = Rc::clone(&device_storage_factory);
            let external_brightness_control =
                controller_flags.contains(&ControllerFlag::ExternalBrightnessControl);
            factory_handle.register(
                SettingType::Display,
                Box::new(
                    move |context| {
                        if external_brightness_control {
                            DataHandler::<DisplayController<T, ExternalBrightnessControl>>::spawn_with_async(
                                context,
                                Rc::clone(&device_storage_factory))
                        } else {
                            DataHandler::<DisplayController<T>>::spawn_with_async(
                                context,
                                Rc::clone(&device_storage_factory))
                        }
                    }
                ),
            );
        }

        // Input
        if components.contains(&SettingType::Input) {
            let input_configuration = Rc::new(std::sync::Mutex::new(
                input_configuration.expect("Input controller requires an input configuration"),
            ));
            device_storage_factory
                .initialize::<InputController<T>>()
                .await
                .expect("storage should still be initializing");
            let device_storage_factory = Rc::clone(&device_storage_factory);
            factory_handle.register(
                SettingType::Input,
                Box::new(move |context| {
                    DataHandler::<InputController<T>>::spawn_with_async(
                        context,
                        (Rc::clone(&device_storage_factory), Rc::clone(&input_configuration)),
                    )
                }),
            );
        }

        // Do not disturb
        if components.contains(&SettingType::DoNotDisturb) {
            device_storage_factory
                .initialize::<DoNotDisturbController<T>>()
                .await
                .expect("storage should still be initializing");
            let device_storage_factory = Rc::clone(&device_storage_factory);
            factory_handle.register(
                SettingType::DoNotDisturb,
                Box::new(move |context| {
                    DataHandler::<DoNotDisturbController<T>>::spawn_with_async(
                        context,
                        Rc::clone(&device_storage_factory),
                    )
                }),
            );
        }

        // Factory Reset
        if components.contains(&SettingType::FactoryReset) {
            device_storage_factory
                .initialize::<FactoryResetController<T>>()
                .await
                .expect("storage should still be initializing");
            let device_storage_factory = Rc::clone(&device_storage_factory);
            factory_handle.register(
                SettingType::FactoryReset,
                Box::new(move |context| {
                    DataHandler::<FactoryResetController<T>>::spawn_with_async(
                        context,
                        Rc::clone(&device_storage_factory),
                    )
                }),
            );
        }
    }
}

fn create_agent_blueprints(
    agent_types: HashSet<AgentType>,
    agent_blueprints: Vec<AgentCreator>,
    media_buttons_event_txs: Vec<UnboundedSender<settings_media_buttons::Event>>,
    setting_value_rx: UnboundedReceiver<(&'static str, String)>,
    external_event_rx: UnboundedReceiver<ExternalServiceEvent>,
    mut usage_router_rx: UnboundedReceiver<UsageEvent>,
) -> Vec<AgentCreator> {
    let (proxy_event_tx, proxy_event_rx) = mpsc::unbounded();
    let (usage_event_tx, usage_event_rx) = mpsc::unbounded();

    // Route general inspect requests to specific inspect agents.
    fasync::Task::local(async move {
        while let Some(usage_event) = usage_router_rx.next().await {
            let _ = proxy_event_tx.unbounded_send(usage_event.clone());
            let _ = usage_event_tx.unbounded_send(usage_event);
        }
    })
    .detach();
    let media_buttons_registrar = agent_types
        .contains(&AgentType::MediaButtons)
        .then(|| agent::media_buttons::create_registrar(media_buttons_event_txs));
    let inspect_settings_values_registrar = agent_types
        .contains(&AgentType::InspectSettingValues)
        .then(|| agent::inspect::setting_values::create_registrar(setting_value_rx));
    let inspect_external_apis_registrar = agent_types
        .contains(&AgentType::InspectExternalApis)
        .then(|| agent::inspect::external_apis::create_registrar(external_event_rx));
    let inspect_setting_proxy_registrar = agent_types
        .contains(&AgentType::InspectSettingProxy)
        .then(|| agent::inspect::setting_proxy::create_registrar(proxy_event_rx));
    let inspect_usages_registrar = agent_types
        .contains(&AgentType::InspectSettingTypeUsage)
        .then(|| agent::inspect::usage_counts::create_registrar(usage_event_rx));

    let agent_registrars = [
        media_buttons_registrar,
        inspect_settings_values_registrar,
        inspect_external_apis_registrar,
        inspect_setting_proxy_registrar,
        inspect_usages_registrar,
    ];

    let mut agent_blueprints = if agent_types.iter().all(|t| {
        matches!(
            t,
            AgentType::MediaButtons
                | AgentType::InspectSettingValues
                | AgentType::InspectExternalApis
                | AgentType::InspectSettingProxy
                | AgentType::InspectSettingTypeUsage
        )
    }) {
        agent_blueprints
    } else {
        agent_types.into_iter().filter_map(AgentCreator::from_type).collect()
    };

    agent_blueprints.extend(agent_registrars.into_iter().filter_map(|r| r));
    agent_blueprints
}

/// Brings up the settings service environment.
///
/// This method generates the necessary infrastructure to support the settings
/// service (handlers, agents, etc.) and brings up the components necessary to
/// support the components specified in the components HashSet.
#[allow(clippy::too_many_arguments)]
async fn create_environment<'a>(
    mut service_dir: ServiceFsDir<'_, ServiceObjLocal<'a, ()>>,
    delegate: service::message::Delegate,
    job_seeder: Seeder,
    components: HashSet<SettingType>,
    registrants: Vec<Registrant>,
    agent_blueprints: Vec<AgentCreator>,
    event_subscriber_blueprints: Vec<event::subscriber::BlueprintHandle>,
    service_context: Rc<ServiceContext>,
    handler_factory: Rc<Mutex<SettingHandlerFactoryImpl>>,
    setting_proxies_node: &fuchsia_inspect::Node,
    listener_logger: Rc<ListenerInspectLogger>,
) -> Result<HashSet<Entity>, Error> {
    for blueprint in event_subscriber_blueprints {
        blueprint.create(delegate.clone()).await;
    }

    let mut entities = HashSet::new();

    for setting_type in &components {
        let _ = SettingProxy::create(
            *setting_type,
            handler_factory.clone(),
            delegate.clone(),
            DEFAULT_SETTING_PROXY_MAX_ATTEMPTS,
            DEFAULT_TEARDOWN_TIMEOUT,
            Some(MonotonicDuration::from_millis(DEFAULT_SETTING_PROXY_RESPONSE_TIMEOUT_MS)),
            true,
            setting_proxies_node.create_child(format!("{setting_type:?}")),
            Rc::clone(&listener_logger),
        )
        .await?;

        let _ = entities.insert(Entity::Handler(*setting_type));
    }

    let mut agent_authority = Authority::create(delegate.clone(), components.clone()).await?;

    for registrant in registrants {
        if registrant.get_dependencies().iter().all(|dependency| {
            let dep_met = dependency.is_fulfilled(&entities);
            if !dep_met {
                log::error!(
                    "Skipping {} registration due to missing dependency {:?}",
                    registrant.get_interface(),
                    dependency
                );
            }
            dep_met
        }) {
            registrant.register(&job_seeder, &mut service_dir);
        }
    }

    for blueprint in agent_blueprints {
        agent_authority.register(blueprint).await;
    }

    // Execute initialization agents sequentially
    agent_authority
        .execute_lifespan(Lifespan::Initialization, Rc::clone(&service_context), true)
        .await
        .context("Agent initialization failed")?;

    // Execute service agents concurrently
    agent_authority
        .execute_lifespan(Lifespan::Service, Rc::clone(&service_context), false)
        .await
        .context("Agent service start failed")?;

    Ok(entities)
}

#[cfg(test)]
mod tests;
