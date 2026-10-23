// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::conversion::{
    convert_health_alert, convert_health_check_error, convert_health_check_request,
    convert_health_check_response, convert_start_args,
};
use crate::loader::{Library, LoaderService};
use crate::modules::ModulesAndSymbols;
use crate::utils::*;
use fdf::{AutoReleaseDispatcher, OnDispatcher};
use fdf_component::Incoming;
use fidl::endpoints::{ClientEnd, RequestStream, ServerEnd};
use fidl_fuchsia_data as fdata;
use fidl_fuchsia_driver_framework as fidl_fdf;
use fidl_fuchsia_driver_host as fdh;
use fidl_fuchsia_power_broker::{
    DependencyToken, ElementControlMarker, ElementRunnerMarker, ElementRunnerRequest,
    LeaseDependency, LeaseSchema, LessorProxy, TopologyProxy,
};
use fidl_next::ClientDispatcher;
use fidl_next_fuchsia_driver_framework as fidl_next_fdf;
use fuchsia_async::TimeoutExt;
use fuchsia_sync::Mutex;
use futures::channel::{mpsc, oneshot};
use futures::future::FusedFuture;
use futures::{FutureExt, StreamExt, TryStreamExt};
use log::warn;
use namespace::Namespace;
use std::collections::HashMap;
use std::ffi::c_char;
use std::ptr::NonNull;
use std::sync::{Arc, Weak};
use zx::Status;

unsafe extern "C" {
    fn driver_host_find_symbol(
        passive_abi: u64,
        so_name: *const c_char,
        so_name_len: usize,
        symbol_name: *const c_char,
        symbol_name_len: usize,
    ) -> u64;

}

type DriverClient = fidl_next::Client<
    fidl_next_fuchsia_driver_framework::Driver,
    fdf_fidl::DriverChannel<fdf::AsyncDispatcher>,
>;

/// The maximum number of health alerts from a driver that can be queued for forwarding to
/// DriverManager. Alerts received while the queue is full are dropped.
const HEALTH_ALERT_QUEUE_CAPACITY: usize = 64;

// Per-level deadlines for a driver to answer a HealthCheck. A check that misses its deadline
// fails with TIMEOUT, which usually means the driver's dispatcher is wedged. These three
// constants mirror the deadlines documented on
// `fuchsia.driver.framework/HealthCheckRequest`; keep them in sync.
const PING_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(250);
const FAST_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(2);
const DEEP_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(120);

struct DriverEventHandler {
    alert_tx: mpsc::Sender<fidl_fdf::HealthAlert>,
    // Number of alerts dropped because the queue was full. Only used to rate-limit the warning.
    dropped: u64,
}

impl<T: fidl_next::Transport> fidl_next_fdf::DriverClientHandler<T> for DriverEventHandler {
    async fn on_health_alert(
        &mut self,
        request: fidl_next::Request<fidl_next_fdf::driver::OnHealthAlert, T>,
    ) {
        let Some(alert) = convert_health_alert(request.payload().alert) else {
            return;
        };
        if let Err(e) = self.alert_tx.try_send(alert)
            && e.is_full()
        {
            // Warn at 1, 2, 4, 8, ... drops so an alert flood doesn't become a log flood. This
            // mirrors the SDK's HealthAlertReporter.
            self.dropped += 1;
            if self.dropped.is_power_of_two() {
                warn!("Health alert queue full; dropped {} alerts so far", self.dropped);
            }
        }
    }
}

struct LoadedDriver(u64);

impl LoadedDriver {
    fn get_hooks(&self, module_name: &str) -> Result<Hooks, Status> {
        const SYMBOL: &str = "__fuchsia_driver_registration__";
        let registration_addr = unsafe {
            driver_host_find_symbol(
                self.0,
                module_name.as_ptr() as *const c_char,
                module_name.len(),
                SYMBOL.as_ptr() as *const c_char,
                SYMBOL.len(),
            )
        };
        if registration_addr == 0 {
            return Err(Status::NOT_FOUND);
        }
        Hooks::new(registration_addr as *mut _)
    }

    fn get_symbols(
        &self,
        program: &fdata::Dictionary,
    ) -> Result<Vec<fidl_fdf::NodeSymbol>, Status> {
        let default = Vec::new();
        let modules = get_program_objvec(program, "modules")?.unwrap_or(&default);

        let mut symbols = Vec::new();

        for module in modules {
            let mut module_name = get_program_string(module, "module_name")?;
            // Special case for compat. The syntax could allow more more generic references to other
            // fields, but we don't need that for now, so we hardcode support for one specific field.
            if module_name == "#program.compat" {
                module_name = get_program_string(program, "compat")?;
            }
            let so_name = basename(module_name);

            // Lookup symbols specific to this module.
            let module_symbols = get_program_strvec(module, "symbols")?.ok_or(Status::NOT_FOUND)?;
            for symbol in module_symbols {
                let address = unsafe {
                    driver_host_find_symbol(
                        self.0,
                        so_name.as_ptr() as *const c_char,
                        so_name.len(),
                        symbol.as_ptr() as *const c_char,
                        symbol.len(),
                    )
                };
                if address == 0 {
                    return Err(Status::INVALID_ARGS);
                }
                symbols.push(fidl_fdf::NodeSymbol {
                    name: Some(symbol.to_string()),
                    address: Some(address),
                    module_name: Some(module_name.to_string()),
                    ..Default::default()
                });
            }
        }

        Ok(symbols)
    }
}

#[derive(Debug)]
struct Hooks(NonNull<fdf_sys::DriverRegistration>);
/// SAFETY: These hooks are just static pointers to code and therefore have no thread local state.
unsafe impl Send for Hooks {}
/// SAFETY: These hooks are valid on every thread after they are loaded.
unsafe impl Sync for Hooks {}

impl Hooks {
    fn new(registration: *mut fdf_sys::DriverRegistration) -> Result<Hooks, Status> {
        if registration.is_null() {
            log::error!("__fuchsia_driver_registration__ symbol not available in driver");
        }
        let registration: NonNull<fdf_sys::DriverRegistration> =
            NonNull::new(registration.cast()).ok_or(Status::NOT_FOUND)?;
        // SAFETY: The symbol is valid as long as the shared library is not closed. So its
        // lifetime must track that of |library| from above. We also do a null check to ensure
        // it's a valid pointer.
        let hooks = unsafe { registration.as_ref() };
        let version = hooks.version;
        if version < 1 || version > fdf_sys::DRIVER_REGISTRATION_VERSION_MAX {
            log::error!("Failed to start driver, unknown driver registration version: {version}");
            return Err(Status::WRONG_TYPE);
        }
        if hooks.v1.initialize.is_none() || hooks.v1.destroy.is_none() {
            log::error!("Failed to start driver, missing methods");
            return Err(Status::WRONG_TYPE);
        }
        Ok(Hooks(registration))
    }

    fn new_from_library(library: &Library) -> Result<Hooks, Status> {
        // SAFETY: The symbol is valid as long as the shared library is not closed. So its
        // lifetime must track that of |library| from above. We also do a null check to ensure
        // it's a valid pointer.
        Hooks::new(unsafe {
            libc::dlsym(library.ptr.as_ptr(), c"__fuchsia_driver_registration__".as_ptr())
        } as *mut _)
    }

    fn initialize(&self, channel_handle: fdf::DriverHandle) -> Token {
        // SAFETY: We know there are 0 other references to this. This is ref-safe because we know
        // the underlying memory is valid for the lifetime of the DriverInner object.
        let hooks = unsafe { self.0.as_ref() };
        let initialize_func = hooks.v1.initialize.unwrap();
        // SAFETY: We know it's safe to call initialize from the initial dispatcher.
        Token(unsafe { initialize_func(channel_handle.into_raw().get()) }.addr())
    }

    /// # Threading
    ///
    /// Must be called in the same sequence as initialize was called.
    fn destroy(&self, token: Token) {
        // SAFETY: We know there are 0 other references to this. This is ref-safe because we know
        // the underlying memory is valid for the lifetime of the DriverInner object.
        let hooks = unsafe { self.0.as_ref() };
        let destroy_func = hooks.v1.destroy.unwrap();
        // SAFETY: We need to call destroy if we've called initialize. This will be
        // synchronized with initialize to occur when the driver is destroyed in the shutdown
        // observer for the dispatcher.
        unsafe { destroy_func(token.0 as *mut _) };
    }
}

#[derive(Debug)]
struct Token(usize);

#[derive(Debug)]
struct LegacyDynamicallyLinkedState {
    #[allow(unused)]
    library: Library,

    // Modules listed in the program's module section.
    #[allow(unused)]
    modules_and_symbols: ModulesAndSymbols,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriverPowerState {
    Resumed,
    SuspendRequested,
    Suspended,
    ResumeRequested,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PowerConfiguration {
    DriverOwned,
    RuntimeControlled,
    NoElement,
}

impl From<&fdata::Dictionary> for PowerConfiguration {
    fn from(program: &fdata::Dictionary) -> Self {
        let power_aware = get_program_string(program, "suspend_enabled")
            .map_or_else(|_| false, |val| matches!(val, "true"));

        let power_managed_dispatchers_enabled =
            get_program_string(program, "power_managed_dispatchers_enabled")
                .map_or_else(|_| false, |val| matches!(val, "true"));

        match (power_aware, power_managed_dispatchers_enabled) {
            (_, true) => PowerConfiguration::RuntimeControlled,
            (true, false) => PowerConfiguration::DriverOwned,
            (false, false) => PowerConfiguration::NoElement,
        }
    }
}

#[derive(Debug)]
struct DriverInner {
    #[allow(unused)]
    legacy_state: Option<LegacyDynamicallyLinkedState>,

    // The hooks to initialize and destroy the driver. Backed by the registration symbol.
    hooks: Hooks,

    // The initial dispatcher of the driver.
    // This is where the initialize hook is called for the driver.
    dispatcher: Option<AutoReleaseDispatcher>,

    // The always-on interface to |dispatcher|.
    always_on_dispatcher: Option<AutoReleaseDispatcher>,

    // This is set through the initialize hook and passed into destroy.
    token: Option<Token>,

    // This signals to the driver_host that the driver has been shutdown.
    shutdown_signaler: Option<oneshot::Sender<Weak<Driver>>>,

    // This is the token representing the node of this driver in the driver manager.
    node_token: Option<fidl::Event>,

    // The power element configuration and state for this driver.
    power_element: PowerElementState,

    // The lease token that we give to the driver's resume hook to hold the
    // power element in an active/fully powered state.
    active_lease: Option<zx::EventPair>,

    // Keeps the power lease active during the stop sequence so the device doesn't
    // lose power until stopping completes.
    stop_lease: Option<zx::EventPair>,

    // The current power state/transition state of the driver.
    power_state: DriverPowerState,

    // A sender used to unblock the termination watcher when a pending resume
    // operation completes or is bypassed.
    resume_sender: Option<oneshot::Sender<()>>,

    // A sender used to signal an internal stop to the termination watcher
    // (e.g. if a power transition fails).
    internal_stop_tx: Option<oneshot::Sender<()>>,

    // The handle to the task running the power element runner for this driver.
    power_element_runner_task: Option<fuchsia_async::Task<()>>,

    resume_requester: Option<fdf_env::ResumeRequesterRegistration>,
}

#[allow(unused)]
#[derive(Debug)]
struct PowerElementHandles {
    control: Option<ClientEnd<ElementControlMarker>>,
    runner: Option<ServerEnd<ElementRunnerMarker>>,
    lessor: LessorProxy,
    element_token: DependencyToken,
    topology: Option<TopologyProxy>,
}

#[derive(Debug)]
enum PowerElementState {
    DriverOwned(Option<ClientEnd<ElementControlMarker>>),
    RuntimeControlled(PowerElementHandles),
    NoElement,
}

impl Drop for DriverInner {
    fn drop(&mut self) {
        self.destroy();
    }
}

impl DriverInner {
    fn new(
        hooks: Hooks,
        legacy_state: Option<LegacyDynamicallyLinkedState>,
        node_token: Option<fidl::Event>,
        power_element: PowerElementState,
    ) -> Self {
        Self {
            legacy_state,
            hooks,
            dispatcher: None,
            always_on_dispatcher: None,
            token: None,
            shutdown_signaler: None,
            node_token,
            power_element,
            active_lease: None,
            stop_lease: None,
            power_state: DriverPowerState::Resumed,
            resume_sender: None,
            internal_stop_tx: None,
            power_element_runner_task: None,
            resume_requester: None,
        }
    }

    fn destroy(&mut self) {
        if let Some(token) = self.token.take() {
            self.hooks.destroy(token);
        }
    }
}

#[derive(Debug)]
pub(crate) struct Driver {
    url: String,
    node_name: String,
    // This is the handle we use to represent the driver to the driver runtime.
    runtime_handle: Mutex<Option<fdf_env::Driver<Driver>>>,
    inner: Mutex<DriverInner>,
}
impl Driver {
    fn process_power_elements(
        power_element_args: Option<fidl_fdf::PowerElementArgs>,
        power_config: PowerConfiguration,
        incoming: &Incoming,
    ) -> (PowerElementState, Option<fidl_fdf::PowerElementArgs>) {
        let Some(args) = power_element_args else {
            return (PowerElementState::NoElement, None);
        };

        let fidl_fdf::PowerElementArgs {
            control_client: Some(control_client),
            runner_server: Some(runner_server),
            lessor_client: Some(lessor_client),
            token: Some(token),
            ..
        } = args
        else {
            return (PowerElementState::NoElement, None);
        };

        match power_config {
            PowerConfiguration::DriverOwned => {
                let power_element = PowerElementState::DriverOwned(Some(control_client));
                let for_driver = Some(fidl_fdf::PowerElementArgs {
                    control_client: None,
                    runner_server: Some(runner_server),
                    lessor_client: Some(lessor_client),
                    token: Some(token),
                    ..Default::default()
                });
                (power_element, for_driver)
            }
            PowerConfiguration::RuntimeControlled => {
                let topology = match incoming.connect_protocol::<TopologyProxy>() {
                    Ok(t) => Some(t),
                    Err(e) => {
                        log::warn!("Failed to connect to Topology via incoming: {e:?}");
                        // Continue setup even if connecting to Topology fails.
                        None
                    }
                };
                let token_clone = token
                    .duplicate_handle(fidl::Rights::SAME_RIGHTS)
                    .expect("Failed to duplicate token");
                let power_element = PowerElementState::RuntimeControlled(PowerElementHandles {
                    control: Some(control_client),
                    runner: Some(runner_server),
                    lessor: lessor_client.into_proxy(),
                    element_token: token,
                    topology,
                });
                let for_driver = Some(fidl_fdf::PowerElementArgs {
                    token: Some(token_clone),
                    ..Default::default()
                });
                (power_element, for_driver)
            }
            PowerConfiguration::NoElement => (PowerElementState::NoElement, None),
        }
    }

    pub async fn load(
        env: &fdf_env::Environment,
        mut start_args: fidl_fdf::DriverStartArgs,
    ) -> Result<(Arc<Driver>, fidl_fdf::DriverStartArgs), Status> {
        // Parse out important
        let url = start_args.url.clone().ok_or(Status::INVALID_ARGS)?;
        let node_name = start_args.node_name.clone().unwrap_or_else(|| "unknown".to_string());
        let program = start_args.program.as_ref().ok_or(Status::INVALID_ARGS)?;
        let binary = get_program_string(program, "binary")?;
        let default_dispatcher_opts = DispatcherOpts::from(program);
        let scheduler_role = get_program_string(program, "default_dispatcher_scheduler_role")
            .unwrap_or("")
            .to_string();
        let allowed_scheduler_roles =
            get_program_strvec(program, "allowed_scheduler_roles")?.cloned();
        let memory_priority_role =
            get_program_string(program, "memory_priority_role").unwrap_or("");

        // Read binary from incoming namespace into vmo.
        let incoming = start_args.incoming.take().ok_or(Status::INVALID_ARGS)?;
        let incoming: Namespace = incoming.try_into().map_err(|_| Status::INVALID_ARGS)?;
        start_args.incoming = Some(incoming.clone().into());
        let incoming: Incoming = incoming.into();
        let vmo = get_file_vmo(&incoming, binary).await?;
        vmo.set_name(&zx::Name::new_lossy(basename(binary)))?;

        if binary != "driver/compat.so" {
            let symbols = driver_symbols::find_restricted_symbols(&vmo, &url)?;
            if !symbols.is_empty() {
                log::error!("Driver '{binary}' referenced restricted symbols: {symbols:?}");
                return Err(Status::NOT_SUPPORTED);
            }
        }
        let vmar = if !memory_priority_role.is_empty() {
            let flags = zx::VmarFlags::CAN_MAP_READ
                | zx::VmarFlags::CAN_MAP_WRITE
                | zx::VmarFlags::CAN_MAP_EXECUTE
                | zx::VmarFlags::CAN_MAP_SPECIFIC;
            // We choose 1GiB as the vmar size fairly arbitrarily. We aren't aware of any drivers
            // that would be larger than that.
            let vmar = fuchsia_runtime::vmar_root_self().allocate(0, 2_usize.pow(30), flags)?.0;
            if let Err(e) = fuchsia_scheduler::set_role_for_vmar(&vmar, memory_priority_role) {
                warn!("Failed to set vmar to priority role {memory_priority_role}: {e:?}");
            }
            Some(vmar)
        } else {
            None
        };

        let power_config = PowerConfiguration::from(program);

        let library = LoaderService::try_load(vmo, &vmar).await?;
        start_args.vmar = vmar;
        let hooks = Hooks::new_from_library(&library)?;
        let modules_and_symbols = ModulesAndSymbols::load(program, &incoming).await?;
        modules_and_symbols.copy_to_start_args(&mut start_args);
        let legacy_state = Some(LegacyDynamicallyLinkedState { library, modules_and_symbols });
        let dispatcher_name = basename(&url);
        let node_token = start_args.node_token.as_ref().map(|t| {
            t.duplicate_handle(fidl::Rights::SAME_RIGHTS)
                .expect("Failed to duplicate node token handle.")
        });

        // All or nothing, either we get all the power element-related handles or we get none.
        let (power_element, for_driver) = Self::process_power_elements(
            start_args.power_element_args.take(),
            power_config,
            &incoming,
        );
        start_args.power_element_args = for_driver;

        let driver = Arc::new(Driver {
            url: url.clone(),
            node_name,
            runtime_handle: Mutex::new(None),
            inner: Mutex::new(DriverInner::new(hooks, legacy_state, node_token, power_element)),
        });
        let driver_runtime_handle = env.new_driver(Arc::into_raw(driver.clone()));

        if !scheduler_role.is_empty() {
            driver_runtime_handle.add_allowed_scheduler_role(&scheduler_role);
        }
        if let Some(roles) = allowed_scheduler_roles {
            for role in roles {
                driver_runtime_handle.add_allowed_scheduler_role(&role);
            }
        }

        // The dispatcher must be shutdown before the dispatcher is destroyed.
        // Usually we will wait for the callback from |fdf_env::DriverShutdown| before destroying
        // the driver object (and hence the dispatcher).
        // In the case where we fail to start the driver, the driver object would be destructed
        // immediately, so here we hold an extra reference to the driver object to ensure the
        // dispatcher will not be destructed until shutdown completes.
        //
        // We do not destroy the dispatcher in the shutdown callback, to prevent crashes that
        // would happen if the driver attempts to access the dispatcher in its Stop hook.
        //
        // Currently we only support synchronized dispatchers for the default dispatcher.
        let driver_clone = driver.clone();
        let dispatcher = fdf::DispatcherBuilder::new()
            .name(&format!("{dispatcher_name}-default-{driver:p}"))
            .scheduler_role(&scheduler_role)
            .shutdown_observer(move |_| {
                let _ = driver_clone;
            });
        let dispatcher = match default_dispatcher_opts {
            DispatcherOpts::AllowSyncCalls => dispatcher.allow_thread_blocking(),
            DispatcherOpts::Default => dispatcher,
        };
        let dispatcher =
            AutoReleaseDispatcher::from(driver_runtime_handle.new_dispatcher(dispatcher)?);
        let always_on_dispatcher = dispatcher.always_on_dispatcher();
        driver.inner.lock().dispatcher = Some(dispatcher);
        driver.inner.lock().always_on_dispatcher = Some(always_on_dispatcher);
        *driver.runtime_handle.lock() = Some(driver_runtime_handle);

        Ok((driver, start_args))
    }

    pub async fn initialize(
        env: &fdf_env::Environment,
        mut start_args: fidl_fdf::DriverStartArgs,
        dynamic_linking_abi: u64,
    ) -> Result<(Arc<Driver>, fidl_fdf::DriverStartArgs), Status> {
        // Parse out important fields.
        let url = start_args.url.clone().ok_or(Status::INVALID_ARGS)?;
        let node_name = start_args.node_name.clone().unwrap_or_else(|| "unknown".to_string());
        let program = start_args.program.as_ref().ok_or(Status::INVALID_ARGS)?;
        let binary = get_program_string(program, "binary")?;
        let default_dispatcher_opts = DispatcherOpts::from(program);
        let scheduler_role =
            get_program_string(program, "default_dispatcher_scheduler_role").unwrap_or("");
        let allowed_scheduler_roles = get_program_strvec(program, "allowed_scheduler_roles")?;

        let power_config = PowerConfiguration::from(program);

        let incoming = start_args.incoming.take().ok_or(Status::INVALID_ARGS)?;
        let incoming: Namespace = incoming.try_into().map_err(|_| Status::INVALID_ARGS)?;
        start_args.incoming = Some(incoming.clone().into());
        let incoming: Incoming = incoming.into();

        let loaded_driver = LoadedDriver(dynamic_linking_abi);
        let hooks = loaded_driver.get_hooks(basename(binary))?;
        let mut symbols = loaded_driver.get_symbols(program)?;
        start_args.symbols.get_or_insert_default().append(&mut symbols);
        let dispatcher_name = basename(&url);
        let node_token = start_args.node_token.as_ref().map(|t| {
            t.duplicate_handle(fidl::Rights::SAME_RIGHTS)
                .expect("Failed to duplicate node token handle.")
        });

        // All or nothing, either we get all the power element-related handles or we get none.
        let (power_element, for_driver) = Self::process_power_elements(
            start_args.power_element_args.take(),
            power_config,
            &incoming,
        );
        start_args.power_element_args = for_driver;

        let driver = Arc::new(Driver {
            url: url.clone(),
            node_name,
            runtime_handle: Mutex::new(None),
            inner: Mutex::new(DriverInner::new(hooks, None, node_token, power_element)),
        });
        let driver_runtime_handle = env.new_driver(Arc::into_raw(driver.clone()));

        if !scheduler_role.is_empty() {
            driver_runtime_handle.add_allowed_scheduler_role(scheduler_role);
        }
        if let Some(roles) = allowed_scheduler_roles {
            for role in roles {
                driver_runtime_handle.add_allowed_scheduler_role(role.as_str());
            }
        }

        // The dispatcher must be shutdown before the dispatcher is destroyed.
        // Usually we will wait for the callback from |fdf_env::DriverShutdown| before destroying
        // the driver object (and hence the dispatcher).
        // In the case where we fail to start the driver, the driver object would be destructed
        // immediately, so here we hold an extra reference to the driver object to ensure the
        // dispatcher will not be destructed until shutdown completes.
        //
        // We do not destroy the dispatcher in the shutdown callback, to prevent crashes that
        // would happen if the driver attempts to access the dispatcher in its Stop hook.
        //
        // Currently we only support synchronized dispatchers for the default dispatcher.
        let driver_clone = driver.clone();
        let dispatcher = fdf::DispatcherBuilder::new()
            .name(&format!("{dispatcher_name}-default-{driver:p}"))
            .scheduler_role(scheduler_role)
            .shutdown_observer(move |_| {
                let _ = driver_clone;
            });
        let dispatcher = match default_dispatcher_opts {
            DispatcherOpts::AllowSyncCalls => dispatcher.allow_thread_blocking(),
            DispatcherOpts::Default => dispatcher,
        };
        let dispatcher =
            AutoReleaseDispatcher::from(driver_runtime_handle.new_dispatcher(dispatcher)?);
        let always_on_dispatcher = dispatcher.always_on_dispatcher();
        driver.inner.lock().dispatcher = Some(dispatcher);
        driver.inner.lock().always_on_dispatcher = Some(always_on_dispatcher);
        *driver.runtime_handle.lock() = Some(driver_runtime_handle);

        Ok((driver, start_args))
    }

    /// Must be called from the driver host main thread.
    pub async fn start(
        self: &Arc<Self>,
        start_args: fidl_fdf::DriverStartArgs,
        driver_request: ServerEnd<fdh::DriverMarker>,
        shutdown_signaler: oneshot::Sender<Weak<Driver>>,
        scope: &fuchsia_async::Scope,
    ) -> Result<(), Status> {
        self.inner.lock().shutdown_signaler = Some(shutdown_signaler);

        let weak_always_on = self
            .inner
            .lock()
            .always_on_dispatcher
            .as_ref()
            .expect("always on dispatcher not set")
            .as_async_dispatcher();
        let (client, server) = fdf_fidl::create_channel_with_dispatchers::<fidl_next_fdf::Driver, _>(
            weak_always_on.clone(),
            weak_always_on.clone(),
        );
        let client_dispatcher = ClientDispatcher::new(client);
        let client = client_dispatcher.client();
        // A futures mpsc channel holds `buffer + number of senders` messages, and there is a
        // single sender.
        let (alert_tx, alert_rx) = mpsc::channel(HEALTH_ALERT_QUEUE_CAPACITY - 1);

        let client_dispatcher_task = {
            let self_clone = self.clone();
            weak_always_on.compute(async move {
                {
                    let mut inner = self_clone.inner.lock();
                    let hooks = &inner.hooks;

                    inner.token =
                        Some(hooks.initialize(server.into_untyped().into_driver_handle()));
                };

                client_dispatcher.run(DriverEventHandler { alert_tx, dropped: 0 }).await.map(|_| ())
            })
        };

        let start_args_next = convert_start_args(start_args);
        let start_result = client.start(start_args_next).await;
        match start_result {
            Ok(Ok(())) => {}
            Ok(Err(status)) => {
                warn!("Driver failed to start: {status:?}");
                client.close();
                if let Err(e) = client_dispatcher_task.await {
                    log::error!("Client dispatcher failed: {e:?}");
                }
                self.shutdown(driver_request);
                return Err(status);
            }
            Err(e) => {
                warn!("Driver start FIDL error: {:?}", e);
                client.close();
                if let Err(e) = client_dispatcher_task.await {
                    log::error!("Client dispatcher failed: {e:?}");
                }
                self.shutdown(driver_request);
                return Err(Status::INTERNAL);
            }
        }

        let (internal_stop_tx, internal_stop_rx) = oneshot::channel::<()>();
        {
            self.inner.lock().internal_stop_tx = Some(internal_stop_tx);
        }

        self.start_power_element_runner(client.clone(), scope);

        let weak_self = Arc::downgrade(self);
        scope.spawn_local(async move {
            Driver::run_termination_watcher(
                weak_self,
                driver_request,
                client_dispatcher_task,
                internal_stop_rx,
                alert_rx,
                client,
            )
            .await;
        });

        Ok(())
    }

    async fn run_termination_watcher<F, O, E>(
        weak_self: Weak<Self>,
        driver_request: ServerEnd<fdh::DriverMarker>,
        client_dispatcher_task: F,
        internal_stop_rx: oneshot::Receiver<()>,
        mut alert_rx: mpsc::Receiver<fidl_fdf::HealthAlert>,
        client: DriverClient,
    ) where
        F: std::future::Future<Output = Result<O, E>> + Send + Unpin + 'static,
        O: std::fmt::Debug,
        E: std::fmt::Debug,
    {
        // Wait for driver manager to issue stop or the driver to have dropped its end of the
        // driver channel.
        let mut driver_request_stream = driver_request.into_stream();
        let mut client_dispatcher_done = client_dispatcher_task.fuse();
        let mut internal_stop_receiver = internal_stop_rx.fuse();

        let (resume_sender, resume_receiver) = oneshot::channel();
        // In-flight health checks are polled concurrently with the request stream and dropped
        // when the loop exits, so a hung check never blocks Stop. An empty `FuturesUnordered`
        // reports itself as terminated, which `select!` skips. The responders are kept out of
        // the futures so that every check still in flight when the loop exits can be answered
        // before the channel is closed (see below).
        let mut health_checks = futures::stream::FuturesUnordered::new();
        let mut health_check_responders: HashMap<u64, fdh::DriverHealthCheckResponder> =
            HashMap::new();
        let mut next_health_check_id: u64 = 0;

        let should_stop = loop {
            futures::select! {
                res = driver_request_stream.try_next().fuse() => {
                    match res {
                        Ok(Some(request)) => match request {
                            fdh::DriverRequest::Stop { control_handle: _ } => {
                                break Driver::handle_stop_request(weak_self.clone(), resume_sender);
                            }
                            fdh::DriverRequest::HealthCheck { request, responder } => {
                                let req_next = convert_health_check_request(request);
                                let timeout = match req_next {
                                    fidl_next_fdf::HealthCheckRequest::Ping => Some(PING_TIMEOUT),
                                    fidl_next_fdf::HealthCheckRequest::Fast => Some(FAST_TIMEOUT),
                                    fidl_next_fdf::HealthCheckRequest::Deep => Some(DEEP_TIMEOUT),
                                    // A level this host does not know has no deadline to
                                    // enforce, so it is not forwarded.
                                    _ => None,
                                };
                                // A suspended driver's dispatchers are stopped by the runtime,
                                // so forwarding the check to the driver's `fdf::Health` server
                                // (via `client` below) would only time out and misreport a
                                // correctly suspended driver as wedged. `driver-host` is the
                                // sole enforcement point for the protocol invariant that
                                // `fdf::Health.HealthCheck` is never sent between `Suspend` and
                                // the completion of `Resume`; the C++ and Rust SDK servers rely
                                // on this and do not track suspend state themselves. The same
                                // gate is applied again when an in-flight check completes (see
                                // the `health_checks` arm below): a check that was forwarded
                                // while resumed but whose deadline ran out while the driver was
                                // suspending is answered INVALID_STATE rather than TIMEOUT.
                                let Some(timeout) = timeout else {
                                    let _ = responder
                                        .send(Err(fidl_fdf::HealthCheckError::NotSupported));
                                    continue;
                                };
                                let power_state =
                                    weak_self.upgrade().map(|driver| driver.inner.lock().power_state);
                                if power_state != Some(DriverPowerState::Resumed) {
                                    log::warn!(
                                        "HealthCheck received while driver is not resumed \
                                         (state: {power_state:?}); answering INVALID_STATE"
                                    );
                                    let _ = responder
                                        .send(Err(fidl_fdf::HealthCheckError::InvalidState));
                                    continue;
                                }
                                let id = next_health_check_id;
                                next_health_check_id += 1;
                                health_check_responders.insert(id, responder);
                                let client_clone = client.clone();
                                health_checks.push(async move {
                                    let res = async move {
                                        match client_clone.health_check(req_next).await {
                                            Ok(Ok(resp)) => {
                                                Ok(convert_health_check_response(resp))
                                            }
                                            Ok(Err(err)) => Err(convert_health_check_error(err)),
                                            // A driver built against an SDK that predates
                                            // HealthCheck answers the flexible method with
                                            // UNKNOWN_METHOD. That is expected, not an error.
                                            Err(fidl_next::Error::Protocol(
                                                fidl_next::ProtocolError::FrameworkError(
                                                    fidl_next::FrameworkError::UnknownMethod,
                                                ),
                                            )) => Err(fidl_fdf::HealthCheckError::NotSupported),
                                            Err(e) => {
                                                log::warn!(
                                                    "Failed to forward HealthCheck to driver: {e:?}"
                                                );
                                                Err(fidl_fdf::HealthCheckError::InternalError)
                                            }
                                        }
                                    }
                                    // On deadline the two-way future is dropped, which cancels
                                    // the transaction so a late reply is discarded. The check
                                    // itself keeps running inside the driver until it finishes
                                    // or the driver stops.
                                    .on_timeout(fuchsia_async::MonotonicInstant::after(timeout), || {
                                        Err(fidl_fdf::HealthCheckError::Timeout)
                                    })
                                    .await;
                                    (id, res)
                                });
                            }
                        },
                        Ok(None) => {
                            log::warn!("Driver request stream closed unexpectedly");
                            break false;
                        }
                        Err(e) => {
                            log::warn!("Error in driver request stream: {e:?}");
                            break false;
                        }
                    }
                }
                (id, res) = health_checks.select_next_some() => {
                    if let Some(responder) = health_check_responders.remove(&id) {
                        // Re-check the gate from the request arm: if the driver left the
                        // resumed state while this check was in flight, its result (typically
                        // a TIMEOUT from stopped dispatchers) says nothing about its health.
                        let resumed = weak_self.upgrade().is_some_and(|driver| {
                            driver.inner.lock().power_state == DriverPowerState::Resumed
                        });
                        let res = if resumed {
                            res
                        } else {
                            Err(fidl_fdf::HealthCheckError::InvalidState)
                        };
                        // Generated Rust FIDL table responders take
                        // `Result<&HealthCheckResponse, HealthCheckError>`.
                        let _ = responder.send(res.as_ref().map_err(|e| *e));
                    }
                }
                alert = alert_rx.select_next_some() => {
                    // A send error means the channel is closed, which the request-stream arm
                    // above already handles by exiting the loop.
                    let _ = driver_request_stream
                        .control_handle()
                        .send_on_health_alert(&alert);
                }
                res = &mut internal_stop_receiver => {
                    log::warn!("Power transition failed, stopping driver internally {:?}", res);
                    // Unblock the resume_receiver.
                    resume_sender.send(()).ok();
                    break true;
                }
                res = &mut client_dispatcher_done => {
                    log::warn!("Client dispatcher finished unexpectedly: {:?}", res);
                    break false;
                }
            }
        };

        // Abandon the in-flight checks, then answer each of them with INVALID_STATE: the driver
        // is no longer running. Every HealthCheck must be answered before the channel is closed
        // below, because the C++ client bindings in driver_manager cannot represent a two-way
        // call that is cut short by a ZX_OK epitaph. This also releases the responders'
        // references before `driver_request_stream` is unwrapped.
        drop(health_checks);
        for (_, responder) in health_check_responders.drain() {
            let _ = responder.send(Err(fidl_fdf::HealthCheckError::InvalidState));
        }
        // Dropping the receiver makes stop-time `try_send` in the event handler fail as
        // disconnected (silently) rather than filling the queue and logging drops.
        drop(alert_rx);

        if should_stop {
            // Ignore the result as its possible we dropped the sender by not assigning it
            // if we did not need to wait for a resume (ie. the driver was already resumed).
            resume_receiver.await.ok();
            if let Err(e) = client.stop().await {
                log::warn!("Failed to send stop request: {e:?}");
                client.close();
            }
            if let Err(e) = client_dispatcher_done.await {
                log::error!("Client dispatcher failed: {e:?}");
            }
        } else if !client_dispatcher_done.is_terminated() {
            client.close();
            if let Err(e) = client_dispatcher_done.await {
                log::error!("Client dispatcher failed: {e:?}");
            }
        }

        if let Some(driver) = weak_self.upgrade() {
            drop(driver.inner.lock().stop_lease.take());
        }

        let server_end = ServerEnd::new(
            Arc::into_inner(driver_request_stream.into_inner().0)
                .expect("outstanding references to channel, possibly unhandeled messages?")
                .into_channel()
                .into(),
        );

        if let Some(strong_self) = weak_self.upgrade() {
            strong_self.shutdown(server_end);
        } else {
            log::error!("Failed to upgrade weak pointer to driver");
        }
    }

    // Called by the driver manager when it wants to stop the driver.
    // Returns true if the Stop lifecycle hook should be called on the driver server.
    fn handle_stop_request(weak_self: Weak<Self>, resume_sender: oneshot::Sender<()>) -> bool {
        let Some(driver) = weak_self.upgrade() else {
            log::warn!("Driver no longer exists");
            return false;
        };

        // Note: We first have to completely resume the driver
        // (if its suspended) before we can stop the driver.
        let state = driver.inner.lock().power_state;
        match state {
            DriverPowerState::Suspended | DriverPowerState::SuspendRequested => {
                driver.inner.lock().resume_sender = Some(resume_sender);
                driver.request_power_lease();
                {
                    let mut inner = driver.inner.lock();
                    inner.stop_lease = inner.active_lease.take();
                }
            }
            DriverPowerState::ResumeRequested => {
                driver.inner.lock().resume_sender = Some(resume_sender);
                {
                    let mut inner = driver.inner.lock();
                    inner.stop_lease = inner.active_lease.take();
                }
            }
            DriverPowerState::Resumed => {
                // Nothing to do here if the driver is already in a Resumed state.
            }
        };

        true
    }

    fn start_power_element_runner(
        self: &Arc<Self>,
        client: DriverClient,
        scope: &fuchsia_async::Scope,
    ) {
        let mut inner = self.inner.lock();
        if let PowerElementState::RuntimeControlled(PowerElementHandles { runner, .. }) =
            &mut inner.power_element
        {
            // Runtime-controlled power element: Deeply integrated with the driver and the
            // driver runtime. Element changes propagate through to the driver runtime to
            // manage task dispatching, and call a lifecycle hook on the driver to manage
            // the state of the hardware.
            inner.power_element_runner_task = Some(self.spawn_power_element_runner(
                runner.take().expect("runner"),
                client,
                scope,
            ));
            drop(inner);

            let weak_self = Arc::downgrade(self);
            let resume_requester = fdf_env::ResumeRequester::new(move || {
                if let Some(driver) = weak_self.upgrade() {
                    driver.request_power_lease();
                    Ok(())
                } else {
                    Err(zx::Status::PEER_CLOSED)
                }
            });

            let registration = self
                .runtime_handle
                .lock()
                .as_ref()
                .expect("driver must have a runtime handle")
                .register_resume_requester(resume_requester);
            self.inner.lock().resume_requester = Some(registration);
        }
    }

    // Signals our local oneshots in case of a failure communicating
    // with the power broker to unblock driver shutdown.
    fn signal_shutdown_fallbacks(&self) {
        let mut inner = self.inner.lock();
        if let Some(sender) = inner.resume_sender.take() {
            sender.send(()).ok();
        }
        if let Some(sender) = inner.internal_stop_tx.take() {
            sender.send(()).ok();
        }
    }

    async fn handle_suspend_transition(self: &Arc<Self>, client: &DriverClient) -> Result<(), ()> {
        {
            let mut inner = self.inner.lock();
            inner.power_state = DriverPowerState::SuspendRequested;
        }
        if self.suspend_driver(client).await.is_err() {
            self.signal_shutdown_fallbacks();
            return Err(());
        }
        {
            let mut inner = self.inner.lock();
            inner.power_state = DriverPowerState::Suspended;
        }
        Ok(())
    }

    async fn handle_resume_transition(self: &Arc<Self>, client: &DriverClient) -> Result<(), ()> {
        let lease = {
            let mut inner = self.inner.lock();

            // Skip resume transition if the driver is already resumed or resume requested.
            if inner.power_state == DriverPowerState::Resumed
                || inner.power_state == DriverPowerState::ResumeRequested
            {
                return Ok(());
            }

            inner.power_state = DriverPowerState::ResumeRequested;
            inner.active_lease.take()
        };

        if self.resume_driver(client, lease).await.is_err() {
            self.signal_shutdown_fallbacks();
            return Err(());
        }

        {
            let mut inner = self.inner.lock();
            inner.power_state = DriverPowerState::Resumed;
            if let Some(sender) = inner.resume_sender.take() {
                sender.send(()).ok();
            }
        }
        Ok(())
    }

    fn spawn_power_element_runner(
        self: &Arc<Self>,
        runner_channel: ServerEnd<ElementRunnerMarker>,
        client: DriverClient,
        scope: &fuchsia_async::Scope,
    ) -> fuchsia_async::Task<()> {
        let mut element_runner_req_stream = runner_channel.into_stream();
        let weak_self = Arc::downgrade(self);
        scope.spawn_local(async move {
            while let Ok(Some(ElementRunnerRequest::SetLevel { level, responder })) =
                element_runner_req_stream.try_next().await
            {
                let Some(driver) = weak_self.upgrade() else {
                    log::warn!("spawn_power_element_runner: driver was freed");
                    break;
                };

                // TODO(https://fxbug.dev/515015623): Use enums instead of 0/1.
                if level == 0 {
                    if driver.handle_suspend_transition(&client).await.is_err() {
                        break;
                    }
                    if let Err(e) = responder.send() {
                        log::warn!("spawn_power_element_runner: failed to send SetLevel(0) response: {:?}", e);
                    }
                } else {
                    if level != 1 {
                        log::error!("Invalid power element level (treating as resume): {}", level);
                    }
                    if driver.handle_resume_transition(&client).await.is_err() {
                        break;
                    }
                    if let Err(e) = responder.send() {
                        log::warn!("spawn_power_element_runner: failed to send SetLevel({}) response: {:?}", level, e);
                    }
                }
            }
        }).into()
    }

    pub fn get_url(&self) -> &str {
        self.url.as_str()
    }

    pub fn duplicate_node_token(&self) -> Option<fidl::Event> {
        self.inner.lock().node_token.as_ref().map(|token| {
            token
                .duplicate_handle(fidl::Rights::SAME_RIGHTS)
                .expect("Failed to duplicate node token handle.")
        })
    }

    /// This function is called when the driver needs to be taken out of the suspended state.
    /// This is only supported when the power element is in `RuntimeControlled` state. This
    /// returns immediately and the work is done asynchronously. Eventually a `SetLevel(1)`
    /// request will arrive via the `ElementRunner` protocol where the driver will resume.
    ///
    /// If the power element is not in `RuntimeControlled` state, this function will do nothing.
    pub fn request_power_lease(self: &Arc<Self>) {
        let self_clone = self.clone();
        let remote_token;
        let dispatcher;
        {
            let mut inner = self.inner.lock();
            let PowerElementState::RuntimeControlled(..) = &inner.power_element else {
                return;
            };

            let (local_token, remote) = zx::EventPair::create();
            inner.active_lease = Some(local_token);
            remote_token = remote;
            dispatcher = inner
                .always_on_dispatcher
                .as_ref()
                .expect("dispatcher should always be valid")
                .as_async_dispatcher();
        }

        // This spawns the request to acquire a power lease to run under the driver dispatcher
        // asynchronously.
        dispatcher.spawn(async move {
            self_clone.acquire_power_lease(remote_token).await;
        });
    }

    async fn acquire_power_lease(&self, remote_token: zx::EventPair) {
        let (element_token, topology) = {
            let inner = self.inner.lock();
            let PowerElementState::RuntimeControlled(PowerElementHandles {
                element_token,
                topology,
                ..
            }) = &inner.power_element
            else {
                return;
            };
            let element_token = element_token
                .duplicate_handle(fidl::Rights::SAME_RIGHTS)
                .expect("Failed to duplicate token");
            let topology = topology.clone();
            (element_token, topology)
        };

        // Lease name limit is 64.
        let mut lease_name = format!("{}:{}", self.node_name, basename(&self.url));
        if lease_name.len() > 64 {
            let cut = lease_name.floor_char_boundary(64);
            lease_name.truncate(cut);
        }

        let lease_schema = LeaseSchema {
            lease_token: Some(remote_token),
            lease_name: Some(lease_name),
            dependencies: Some(vec![LeaseDependency {
                requires_token: Some(element_token),
                requires_level: Some(1),
                ..Default::default()
            }]),
            should_return_pending_lease: Some(true),
            ..Default::default()
        };

        if let Some(topology) = topology {
            match topology.lease(lease_schema).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    log::error!("Failed to request lease: {e:?}");
                    self.signal_shutdown_fallbacks();
                }
                Err(e) => {
                    log::error!("FIDL error requesting lease: {e:?}");
                    self.signal_shutdown_fallbacks();
                }
            }
        } else {
            log::error!("Topology proxy not available");
            self.signal_shutdown_fallbacks();
        }
    }

    async fn suspend_driver(self: &Arc<Self>, client: &DriverClient) -> Result<(), ()> {
        // Suspending a driver involves two steps:
        // 1: Suspend the driver from the perspective of the driver runtime. This will put the
        // dispatchers into the suspended state ensuring no more work will be performed for them.
        // 2: Send the suspend call to the driver via the Driver protocol. This will give the
        // driver the opportunity to suspend its hardware. This runs on the always-on dispatcher
        // so it bypasses the suspend we do in step 1.

        let (completer_sender, completer_receiver) = oneshot::channel();
        let completer = fdf_env::SuspendCompleter::new(move || {
            completer_sender.send(()).ok();
        });

        self.runtime_handle
            .lock()
            .as_ref()
            .expect("driver must have a runtime handle")
            .driver_suspend(completer);

        match completer_receiver.await {
            Ok(()) => match client.suspend().await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => {
                    log::error!("Driver suspend application error for {}: {:?}", self.url, e,);
                    Err(())
                }
                Err(e) => {
                    log::error!("Driver suspend FIDL error for {}: {:?}", self.url, e);
                    Err(())
                }
            },
            Err(_) => {
                log::error!("Failed to receive suspend completion for {}", self.url);
                Err(())
            }
        }
    }

    async fn resume_driver(
        self: &Arc<Self>,
        client: &DriverClient,
        lease: Option<zx::EventPair>,
    ) -> Result<(), ()> {
        // Resuming a driver involves two steps:
        // 1: Send the resume call to the driver via the Driver protocol. This will give the
        // driver the opportunity to resume its hardware. This runs on the always-on dispatcher
        // so it bypasses the suspend we do in step 1 of suspend_driver.
        // 2: Resume the driver from the perspective of the driver runtime. This will put the
        // dispatchers into the running state ensuring that work is once again processed.
        match client.resume(lease).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                log::error!("Driver resume application error for {}: {:?}", self.url, e);
                return Err(());
            }
            Err(e) => {
                log::error!("Driver resume FIDL error for {}: {:?}", self.url, e);
                return Err(());
            }
        }

        self.runtime_handle
            .lock()
            .as_ref()
            .expect("driver must have a runtime handle")
            .driver_resume();
        Ok(())
    }

    /// Shutdown the driver. The process is asynchronous. All references to the driver should be
    /// dropped prior to invoking this method.
    /// This method will do nothing if invoked multiple times or if runtime_handle was never
    /// initialized.
    fn shutdown(&self, driver_request: ServerEnd<fdh::DriverMarker>) {
        // Drop the power element runner task to prevent it from sending any
        // more requests on the runtime_handle which we are about to take.
        drop(self.inner.lock().power_element_runner_task.take());

        // Clean up our resume requester from the runtime. This is only available
        // in drivers that have RuntimeControlled power_elements.
        if let Some(registration) = self.inner.lock().resume_requester.take() {
            registration.unregister();
        }

        let runtime_handle = self.runtime_handle.lock().take();
        let shutdown_signaler = self.inner.lock().shutdown_signaler.take();

        // Drop the element control channel to destroy the power element
        match &mut self.inner.lock().power_element {
            PowerElementState::RuntimeControlled(PowerElementHandles { control, .. })
            | PowerElementState::DriverOwned(control) => {
                drop(control.take());
            }
            _ => {}
        }

        if let Some(runtime_handle) = runtime_handle {
            runtime_handle.shutdown(move |runtime_handle| {
                // SAFETY: This is safe because we previously leaked the arc when creating the
                // driver. Recovering through the shutdown callback is the expected flow.
                let this = unsafe { Arc::from_raw(runtime_handle.0) };
                if let Some(shutdown_signaler) = shutdown_signaler {
                    // This can fail if start fails.
                    shutdown_signaler.send(Arc::downgrade(&this)).ok();
                }

                // Trigger destroy hook.
                // In theory this should be the last reference to the driver, however if shutdown
                // is invoked from the main driver_host thread, it's not guaranteed.
                this.inner.lock().destroy();

                let _ = driver_request.close_with_epitaph(Ok(()));
            });
        };
    }
}

impl PartialEq<fdf_env::UnownedDriver> for Driver {
    fn eq(&self, other: &fdf_env::UnownedDriver) -> bool {
        self.runtime_handle.lock().as_ref().map(|h| h == other).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdf::{DispatcherBuilder, OnDriverDispatcher, WakeVectorRegistration};
    use fidl_fuchsia_power_broker::{
        LessorMarker, TopologyMarker, TopologyRequest, TopologyRequestStream,
    };
    use libasync::OnSignals;
    use std::pin::Pin;
    use std::sync::mpsc;
    use std::task::{Context, Wake, Waker};

    unsafe extern "C" fn fake_initialize(
        _server_handle: fdf_sys::fdf_handle_t,
    ) -> *mut std::ffi::c_void {
        std::ptr::null_mut()
    }

    unsafe extern "C" fn fake_destroy(_token: *mut std::ffi::c_void) {}

    // The tests never initialize the driver, so these hooks are never called.
    static FAKE_REGISTRATION: fdf_sys::DriverRegistration = fdf_sys::DriverRegistration {
        version: fdf_sys::DRIVER_REGISTRATION_VERSION_MAX,
        v1: fdf_sys::DriverRegistration_driver_registration_v1 {
            initialize: Some(fake_initialize),
            destroy: Some(fake_destroy),
        },
    };

    /// The power broker that the power element of a test driver is connected to.
    struct FakePowerBroker {
        topology: TopologyRequestStream,
        // Keep the other ends of the power element channels open.
        _runner: ClientEnd<ElementRunnerMarker>,
        _lessor: ServerEnd<LessorMarker>,
    }

    impl FakePowerBroker {
        /// Waits for the driver host to lease the driver's power element and grants the lease.
        async fn expect_lease(&mut self) -> LeaseSchema {
            match self.topology.try_next().await {
                Ok(Some(TopologyRequest::Lease { payload, responder })) => {
                    responder.send(Ok(())).expect("failed to respond to lease request");
                    payload
                }
                other => panic!("expected a lease request, got {other:?}"),
            }
        }
    }

    /// Creates a driver with a runtime controlled power element and registers it with the driver
    /// runtime the way `Driver::load` does, without loading a driver library.
    fn create_test_driver(env: &fdf_env::Environment) -> (Arc<Driver>, FakePowerBroker) {
        let (runner_client, runner_server) =
            fidl::endpoints::create_endpoints::<ElementRunnerMarker>();
        let (lessor, lessor_server) = fidl::endpoints::create_proxy::<LessorMarker>();
        let (topology, topology_stream) =
            fidl::endpoints::create_proxy_and_stream::<TopologyMarker>();
        let hooks = Hooks::new(std::ptr::addr_of!(FAKE_REGISTRATION).cast_mut()).unwrap();
        let driver = Arc::new(Driver {
            url: "fuchsia-boot:///#meta/test-driver.cm".to_string(),
            node_name: "test-node".to_string(),
            runtime_handle: Mutex::new(None),
            inner: Mutex::new(DriverInner {
                legacy_state: None,
                hooks,
                dispatcher: None,
                always_on_dispatcher: None,
                token: None,
                shutdown_signaler: None,
                node_token: None,
                power_element: PowerElementState::RuntimeControlled(PowerElementHandles {
                    control: None,
                    runner: Some(runner_server),
                    lessor,
                    element_token: zx::Event::create(),
                    topology: Some(topology),
                }),
                active_lease: None,
                stop_lease: None,
                power_state: DriverPowerState::Resumed,
                resume_sender: None,
                internal_stop_tx: None,
                power_element_runner_task: None,
                resume_requester: None,
            }),
        });
        let runtime_handle = env.new_driver(Arc::into_raw(driver.clone()));
        // The wake vector callbacks in these tests block, so the dispatcher has to allow that.
        let dispatcher = runtime_handle
            .new_dispatcher(
                DispatcherBuilder::new().name("test-driver-default").allow_thread_blocking(),
            )
            .unwrap();
        let dispatcher = AutoReleaseDispatcher::from(dispatcher);
        {
            let mut inner = driver.inner.lock();
            inner.always_on_dispatcher = Some(dispatcher.always_on_dispatcher());
            inner.dispatcher = Some(dispatcher);
        }
        *driver.runtime_handle.lock() = Some(runtime_handle);
        let power_broker = FakePowerBroker {
            topology: topology_stream,
            _runner: runner_client,
            _lessor: lessor_server,
        };
        (driver, power_broker)
    }

    /// Creates a client for the driver's `fuchsia.driver.framework/Driver` protocol. Nothing serves
    /// the protocol because the tests never get to the point of calling into the driver.
    fn create_driver_client(driver: &Driver) -> DriverClient {
        let always_on =
            driver.inner.lock().always_on_dispatcher.as_ref().unwrap().as_async_dispatcher();
        let (client_end, _server_end) = fdf_fidl::create_channel_with_dispatchers::<
            fidl_next_fdf::Driver,
            _,
        >(always_on.clone(), always_on);
        ClientDispatcher::new(client_end).client()
    }

    /// Shuts down a driver created by `create_test_driver` and waits until it is gone.
    async fn shutdown_test_driver(driver: Arc<Driver>, scope: fuchsia_async::Scope) {
        // The driver runtime doesn't shut down suspended dispatchers, so resume them first, like
        // the driver host does before it stops a driver.
        driver.runtime_handle.lock().as_ref().unwrap().driver_resume();
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        driver.inner.lock().shutdown_signaler = Some(shutdown_sender);
        let (_driver_client, driver_server) =
            fidl::endpoints::create_endpoints::<fdh::DriverMarker>();
        driver.shutdown(driver_server);
        shutdown_receiver.await.expect("driver shutdown did not complete");
        // The tasks and channels of the driver are registered with the test's executor, so they
        // must be dropped on this thread before the executor goes away. The shutdown only aborted
        // the power element runner task, and the driver runtime threads release their references
        // to the driver asynchronously.
        scope.abort().await;
        while Arc::strong_count(&driver) > 1 {
            fuchsia_async::Timer::new(fuchsia_async::MonotonicDuration::from_millis(1)).await;
        }
    }

    /// A waker that blocks the thread that wakes it until it is released.
    struct BlockingWaker {
        entered: Mutex<Option<oneshot::Sender<()>>>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl Wake for BlockingWaker {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            let Some(entered) = self.entered.lock().take() else {
                return;
            };
            let _ = entered.send(());
            // This returns once the waker is released or the release sender is dropped.
            let _ = self.release.lock().recv();
        }
    }

    /// A wake vector callback that keeps executing on a driver dispatcher until it is released.
    struct InFlightWakeVector<'a> {
        release: mpsc::Sender<()>,
        _wait: OnSignals<&'a zx::Event>,
        _registration: WakeVectorRegistration,
    }

    impl InFlightWakeVector<'_> {
        /// Lets the wake vector callback return.
        fn release(self) {
            self.release.send(()).expect("wake vector callback is gone");
        }
    }

    /// Registers `event` as a wake vector of `dispatcher`, waits on it on `dispatcher` and
    /// signals it. Returns once the callback of the wait is executing, which it keeps doing until
    /// it is released.
    async fn start_wake_vector_callback<'a>(
        dispatcher: &fdf::AsyncDispatcher,
        event: &'a zx::Event,
    ) -> InFlightWakeVector<'a> {
        let registration = dispatcher.register_wake_vector(event, zx::Signals::USER_0).unwrap();
        let (entered_sender, entered_receiver) = oneshot::channel();
        let (release, release_receiver) = mpsc::channel();
        let waker = Waker::from(Arc::new(BlockingWaker {
            entered: Mutex::new(Some(entered_sender)),
            release: Mutex::new(release_receiver),
        }));
        // The callback of the wait wakes `waker`, which blocks it.
        let mut wait = OnSignals::new_on(dispatcher.clone(), event, zx::Signals::USER_0);
        assert!(Pin::new(&mut wait).poll(&mut Context::from_waker(&waker)).is_pending());
        event.signal(zx::Signals::empty(), zx::Signals::USER_0).unwrap();
        entered_receiver.await.expect("wake vector callback did not run");
        InFlightWakeVector { release, _wait: wait, _registration: registration }
    }

    // When a driver is suspended while one of its wake vector callbacks is executing, the driver
    // runtime calls the resume requester from within `fdf_env_driver_suspend`. That must neither
    // deadlock nor be lost.
    #[fuchsia::test]
    async fn suspend_with_wake_vector_in_flight_requests_power_lease() {
        let env = fdf_env::Environment::start(0).unwrap();
        let (driver, mut power_broker) = create_test_driver(&env);
        let client = create_driver_client(&driver);
        let scope = fuchsia_async::Scope::new();
        driver.start_power_element_runner(client.clone(), &scope);

        let dispatcher = driver.inner.lock().dispatcher.as_ref().unwrap().as_async_dispatcher();
        let event = zx::Event::create();
        let wake_vector = start_wake_vector_callback(&dispatcher, &event).await;

        // Suspending the runtime only completes once the wake vector callback returns, so this
        // stays pending. It is dropped there since nothing serves the driver protocol, which is
        // what it would use next.
        assert!(driver.suspend_driver(&client).now_or_never().is_none());

        wake_vector.release();
        let lease = power_broker.expect_lease().await;
        assert_eq!(lease.lease_name.as_deref(), Some("test-node:test-driver.cm"));
        assert!(driver.inner.lock().active_lease.is_some());

        shutdown_test_driver(driver, scope).await;
    }

    // When the resume requester is registered while the driver is suspended and one of its wake
    // vector callbacks is executing, the driver runtime calls the requester from within
    // `fdf_env_register_resume_requester`. That must neither deadlock nor be lost.
    #[fuchsia::test]
    async fn start_power_element_runner_while_suspended_with_wake_vector_in_flight() {
        let env = fdf_env::Environment::start(0).unwrap();
        let (driver, mut power_broker) = create_test_driver(&env);
        let client = create_driver_client(&driver);

        let dispatcher = driver.inner.lock().dispatcher.as_ref().unwrap().as_async_dispatcher();
        let event = zx::Event::create();
        let wake_vector = start_wake_vector_callback(&dispatcher, &event).await;

        // No resume requester is registered yet, so the driver runtime can't request a resume.
        assert!(driver.suspend_driver(&client).now_or_never().is_none());

        let scope = fuchsia_async::Scope::new();
        driver.start_power_element_runner(client, &scope);

        wake_vector.release();
        power_broker.expect_lease().await;
        assert!(driver.inner.lock().active_lease.is_some());

        shutdown_test_driver(driver, scope).await;
    }

    /// The transport of the lifecycle server of the fake driver below.
    type FailingStartTransport = fdf_fidl::DriverChannel<fdf::AsyncDispatcher>;

    /// A `fuchsia.driver.framework/Driver` server that fails `Start`. Nothing else is ever
    /// called on it because the driver host doesn't talk to a driver that failed to start.
    struct FailingStartHandler;

    impl fidl_next_fdf::DriverServerHandler<FailingStartTransport> for FailingStartHandler {
        async fn start(
            &mut self,
            _request: fidl_next::Request<fidl_next_fdf::driver::Start, FailingStartTransport>,
            responder: fidl_next::Responder<fidl_next_fdf::driver::Start, FailingStartTransport>,
        ) {
            let _ = responder.respond_err(Status::INTERNAL).await;
        }

        async fn stop(&mut self) {
            unreachable!("stop is not sent to a driver that failed to start");
        }

        async fn suspend(
            &mut self,
            _responder: fidl_next::Responder<fidl_next_fdf::driver::Suspend, FailingStartTransport>,
        ) {
            unreachable!("suspend is not sent to a driver that failed to start");
        }

        async fn resume(
            &mut self,
            _request: fidl_next::Request<fidl_next_fdf::driver::Resume, FailingStartTransport>,
            _responder: fidl_next::Responder<fidl_next_fdf::driver::Resume, FailingStartTransport>,
        ) {
            unreachable!("resume is not sent to a driver that failed to start");
        }

        async fn health_check(
            &mut self,
            _request: fidl_next::Request<fidl_next_fdf::driver::HealthCheck, FailingStartTransport>,
            _responder: fidl_next::Responder<
                fidl_next_fdf::driver::HealthCheck,
                FailingStartTransport,
            >,
        ) {
            unreachable!("health check is not sent to a driver that failed to start");
        }
    }

    unsafe extern "C" fn failing_start_initialize(
        server_handle: fdf_sys::fdf_handle_t,
    ) -> *mut std::ffi::c_void {
        let root_dispatcher = unsafe {
            fdf::DriverDispatcherRef::from_raw(
                NonNull::new(fdf_sys::fdf_dispatcher_get_current_dispatcher())
                    .expect("Non-null current dispatcher"),
            )
        };
        let server_channel = unsafe {
            fdf::Channel::<[fidl_next::Chunk]>::from_driver_handle(
                fdf::DriverHandle::new_unchecked(
                    std::num::NonZero::new(server_handle).expect("valid driver handle"),
                ),
            )
        };
        let always_on = root_dispatcher.always_on_dispatcher();
        let server_end: fidl_next::ServerEnd<fidl_next_fdf::Driver, FailingStartTransport> =
            fidl_next::ServerEnd::from_untyped(fdf_fidl::DriverChannel::new_with_dispatcher(
                fdf::AsyncDispatcher::new(&always_on),
                server_channel,
            ));
        // The server task owns the channel and keeps it open after the failed `Start` reply until
        // the driver host closes its end or shuts the driver down, matching C++ `DriverServer2`
        // where the server binding outlives a failed `Start` reply.
        always_on.spawn(async move {
            let _ = fidl_next::ServerDispatcher::new(server_end).run(FailingStartHandler).await;
        });
        std::ptr::null_mut()
    }

    unsafe extern "C" fn failing_start_destroy(_token: *mut std::ffi::c_void) {}

    static FAILING_START_REGISTRATION: fdf_sys::DriverRegistration = fdf_sys::DriverRegistration {
        version: fdf_sys::DRIVER_REGISTRATION_VERSION_MAX,
        v1: fdf_sys::DriverRegistration_driver_registration_v1 {
            initialize: Some(failing_start_initialize),
            destroy: Some(failing_start_destroy),
        },
    };

    #[fuchsia::test]
    async fn start_failure_closes_client_before_shutdown() {
        let env = fdf_env::Environment::start(0).unwrap();
        let scope = fuchsia_async::Scope::new();
        for _ in 0..20 {
            let (driver, _power_broker) = create_test_driver(&env);
            driver.inner.lock().hooks =
                Hooks::new(std::ptr::addr_of!(FAILING_START_REGISTRATION).cast_mut()).unwrap();
            let (shutdown_sender, shutdown_receiver) = oneshot::channel();
            let (_driver_client, driver_server) =
                fidl::endpoints::create_endpoints::<fdh::DriverMarker>();
            let status = driver
                .start(fidl_fdf::DriverStartArgs::default(), driver_server, shutdown_sender, &scope)
                .await
                .expect_err("expected Driver::start to fail");
            assert_eq!(status, Status::INTERNAL);
            shutdown_receiver.await.expect("driver shutdown did not complete");
            while Arc::strong_count(&driver) > 1 {
                fuchsia_async::Timer::new(fuchsia_async::MonotonicDuration::from_millis(1)).await;
            }
        }
        scope.abort().await;
    }

    #[fuchsia::test]
    fn power_configuration_from_program() {
        let empty = fdata::Dictionary::default();
        assert_eq!(PowerConfiguration::from(&empty), PowerConfiguration::NoElement);

        let false_both = fdata::Dictionary {
            entries: Some(vec![
                fdata::DictionaryEntry {
                    key: "suspend_enabled".to_string(),
                    value: Some(Box::new(fdata::DictionaryValue::Str("false".to_string()))),
                },
                fdata::DictionaryEntry {
                    key: "power_managed_dispatchers_enabled".to_string(),
                    value: Some(Box::new(fdata::DictionaryValue::Str("false".to_string()))),
                },
            ]),
            ..Default::default()
        };
        assert_eq!(PowerConfiguration::from(&false_both), PowerConfiguration::NoElement);

        let driver_owned = fdata::Dictionary {
            entries: Some(vec![fdata::DictionaryEntry {
                key: "suspend_enabled".to_string(),
                value: Some(Box::new(fdata::DictionaryValue::Str("true".to_string()))),
            }]),
            ..Default::default()
        };
        assert_eq!(PowerConfiguration::from(&driver_owned), PowerConfiguration::DriverOwned);

        let runtime_controlled = fdata::Dictionary {
            entries: Some(vec![fdata::DictionaryEntry {
                key: "power_managed_dispatchers_enabled".to_string(),
                value: Some(Box::new(fdata::DictionaryValue::Str("true".to_string()))),
            }]),
            ..Default::default()
        };
        assert_eq!(
            PowerConfiguration::from(&runtime_controlled),
            PowerConfiguration::RuntimeControlled
        );
    }

    #[fuchsia::test]
    async fn process_power_elements_respects_configuration() {
        let incoming: Incoming = namespace::Namespace::new().into();

        // 1. NoElement with no handles -> NoElement
        let (state, for_driver) =
            Driver::process_power_elements(None, PowerConfiguration::NoElement, &incoming);
        assert!(matches!(state, PowerElementState::NoElement));
        assert!(for_driver.is_none());

        // 2. DriverOwned with handles -> DriverOwned, runner and lessor remain in for_driver
        let (control_client, _control_server) =
            fidl::endpoints::create_endpoints::<fidl_fuchsia_power_broker::ElementControlMarker>();
        let (_runner_client, runner_server) =
            fidl::endpoints::create_endpoints::<ElementRunnerMarker>();
        let (lessor_client, _lessor_server) =
            fidl::endpoints::create_endpoints::<fidl_fuchsia_power_broker::LessorMarker>();
        let pe_args = fidl_fdf::PowerElementArgs {
            control_client: Some(control_client),
            runner_server: Some(runner_server),
            lessor_client: Some(lessor_client),
            token: Some(zx::Event::create()),
            ..Default::default()
        };
        let (state, for_driver) = Driver::process_power_elements(
            Some(pe_args),
            PowerConfiguration::DriverOwned,
            &incoming,
        );
        assert!(matches!(state, PowerElementState::DriverOwned(Some(_))));
        let pe_args = for_driver.as_ref().unwrap();
        assert!(pe_args.control_client.is_none());
        assert!(pe_args.runner_server.is_some());
        assert!(pe_args.lessor_client.is_some());
        assert!(pe_args.token.is_some());

        // 3. RuntimeControlled with handles -> RuntimeControlled, only token remains in for_driver
        let (control_client, _control_server) =
            fidl::endpoints::create_endpoints::<fidl_fuchsia_power_broker::ElementControlMarker>();
        let (_runner_client, runner_server) =
            fidl::endpoints::create_endpoints::<ElementRunnerMarker>();
        let (lessor_client, _lessor_server) =
            fidl::endpoints::create_endpoints::<fidl_fuchsia_power_broker::LessorMarker>();
        let pe_args = fidl_fdf::PowerElementArgs {
            control_client: Some(control_client),
            runner_server: Some(runner_server),
            lessor_client: Some(lessor_client),
            token: Some(zx::Event::create()),
            ..Default::default()
        };
        let (state, for_driver) = Driver::process_power_elements(
            Some(pe_args),
            PowerConfiguration::RuntimeControlled,
            &incoming,
        );
        assert!(matches!(state, PowerElementState::RuntimeControlled(_)));
        let pe_args = for_driver.as_ref().unwrap();
        assert!(pe_args.control_client.is_none());
        assert!(pe_args.runner_server.is_none());
        assert!(pe_args.lessor_client.is_none());
        assert!(pe_args.token.is_some());

        // 4. NoElement even if handles are provided -> NoElement and None for driver
        let (control_client, _control_server) =
            fidl::endpoints::create_endpoints::<fidl_fuchsia_power_broker::ElementControlMarker>();
        let (_runner_client, runner_server) =
            fidl::endpoints::create_endpoints::<ElementRunnerMarker>();
        let (lessor_client, _lessor_server) =
            fidl::endpoints::create_endpoints::<fidl_fuchsia_power_broker::LessorMarker>();
        let pe_args = fidl_fdf::PowerElementArgs {
            control_client: Some(control_client),
            runner_server: Some(runner_server),
            lessor_client: Some(lessor_client),
            token: Some(zx::Event::create()),
            ..Default::default()
        };
        let (state, for_driver) =
            Driver::process_power_elements(Some(pe_args), PowerConfiguration::NoElement, &incoming);
        assert!(matches!(state, PowerElementState::NoElement));
        assert!(for_driver.is_none());
    }
}

#[cfg(test)]
mod health_check_tests {
    use super::*;
    use fdf::OnDispatcher;
    use fidl_next::{Request, Responder, ServerDispatcher, ServerEnd as NextServerEnd};
    use fidl_next_fuchsia_driver_framework::{DriverServerHandler, driver};
    use std::ffi::c_void;
    use std::num::NonZero;
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    static SUSPEND_TEST_HEALTH_CHECK_CALLS: AtomicUsize = AtomicUsize::new(0);

    type Transport = fdf_fidl::DriverChannel<fdf::AsyncDispatcher>;

    struct MockDriverServer {
        count_calls: AtomicBool,
        fidl_server: fidl_next::Server<fidl_next_fdf::Driver, Transport>,
        hung_responders: Vec<Responder<driver::HealthCheck, Transport>>,
    }

    impl DriverServerHandler<Transport> for MockDriverServer {
        async fn start(
            &mut self,
            request: Request<driver::Start, Transport>,
            responder: Responder<driver::Start, Transport>,
        ) {
            let count_calls = request.payload().start_args.url.as_deref() == Some("suspend-test");
            self.count_calls.store(count_calls, Ordering::SeqCst);
            let _ = responder.respond(()).await;
        }

        async fn stop(&mut self) {
            self.fidl_server.close();
        }

        async fn suspend(&mut self, responder: Responder<driver::Suspend, Transport>) {
            let _ = responder.respond(()).await;
        }

        async fn resume(
            &mut self,
            _request: Request<driver::Resume, Transport>,
            responder: Responder<driver::Resume, Transport>,
        ) {
            let _ = responder.respond(()).await;
        }

        async fn health_check(
            &mut self,
            request: Request<driver::HealthCheck, Transport>,
            responder: Responder<driver::HealthCheck, Transport>,
        ) {
            if self.count_calls.load(Ordering::SeqCst) {
                SUSPEND_TEST_HEALTH_CHECK_CALLS.fetch_add(1, Ordering::SeqCst);
            }
            match request.payload().request {
                fidl_next_fdf::HealthCheckRequest::Ping => {
                    // Simulate a hung driver to exercise the Ping timeout in driver-host.
                    // Hold onto the responder without replying so `ServerDispatcher` can still
                    // process subsequent requests (such as `Stop`) and `Responder::drop` does not
                    // close the channel early.
                    self.hung_responders.push(responder);
                }
                fidl_next_fdf::HealthCheckRequest::Deep => {
                    let _ =
                        responder.respond_err(fidl_next_fdf::HealthCheckError::NotSupported).await;
                }
                _ => {
                    let _ = self
                        .fidl_server
                        .on_health_alert(fidl_next_fdf::HealthAlert::UpstreamNeighborFault(
                            fidl_next_fdf::HealthCheckResponse {
                                conditions: Some(vec![fidl_next_fdf::HealthCondition {
                                    status: Some(fidl_next_fdf::HealthStatus::Degraded),
                                    reason_code: Some("LINK_TIMEOUT".to_string()),
                                    message: Some("neighbor link timed out".to_string()),
                                    ..Default::default()
                                }]),
                                ..Default::default()
                            },
                        ))
                        .await;
                    let _ = responder
                        .respond(fidl_next_fdf::HealthCheckResponse {
                            conditions: Some(vec![fidl_next_fdf::HealthCondition {
                                status: Some(fidl_next_fdf::HealthStatus::Degraded),
                                reason_code: Some("HOST_FORWARD_OK".to_string()),
                                last_observed: Some(7),
                                ..Default::default()
                            }]),
                            ..Default::default()
                        })
                        .await;
                }
            }
        }
    }

    unsafe extern "C" fn mock_initialize(server_handle: fdf_sys::fdf_handle_t) -> *mut c_void {
        let root_dispatcher =
            NonNull::new(unsafe { fdf_sys::fdf_dispatcher_get_current_dispatcher() })
                .expect("Non-null current dispatcher");
        let channel = unsafe {
            fdf::Channel::<[fidl_next::Chunk]>::from_driver_handle(
                fdf::DriverHandle::new_unchecked(
                    NonZero::new(server_handle).expect("valid driver handle"),
                ),
            )
        };
        let root_dispatcher = unsafe { fdf::DriverDispatcherRef::from_raw(root_dispatcher) };
        let always_on = fdf::AsyncDispatcher::new(&root_dispatcher.always_on_dispatcher());
        let server_end: NextServerEnd<fidl_next_fdf::Driver, Transport> =
            NextServerEnd::from_untyped(fdf_fidl::DriverChannel::new_with_dispatcher(
                always_on.clone(),
                channel,
            ));
        let dispatcher = ServerDispatcher::new(server_end);
        let handler = MockDriverServer {
            count_calls: AtomicBool::new(false),
            fidl_server: dispatcher.server(),
            hung_responders: Vec::new(),
        };
        always_on.spawn(async move {
            let _ = dispatcher.run(handler).await;
        });
        std::ptr::dangling_mut()
    }

    unsafe extern "C" fn mock_destroy(_token: *mut c_void) {}

    static MOCK_REGISTRATION: fdf_sys::DriverRegistration = fdf_sys::DriverRegistration {
        version: fdf_sys::DRIVER_REGISTRATION_VERSION_MAX,
        v1: fdf_sys::DriverRegistration_driver_registration_v1 {
            initialize: Some(mock_initialize),
            destroy: Some(mock_destroy),
        },
    };

    fn create_test_driver(env: &fdf_env::Environment) -> Arc<Driver> {
        // Hooks::new only reads through the pointer, so casting away const is sound here.
        let hooks = Hooks::new((&raw const MOCK_REGISTRATION) as *mut _).unwrap();
        let driver = Arc::new(Driver {
            url: "fuchsia-boot:///#meta/mock.cm".to_string(),
            node_name: "mock".to_string(),
            runtime_handle: Mutex::new(None),
            inner: Mutex::new(DriverInner::new(hooks, None, None, PowerElementState::NoElement)),
        });
        let driver_runtime_handle = env.new_driver(Arc::into_raw(driver.clone()));
        let driver_clone = driver.clone();
        let dispatcher = AutoReleaseDispatcher::from(
            driver_runtime_handle
                .new_dispatcher(
                    fdf::DispatcherBuilder::new().name("mock-default").shutdown_observer(
                        move |_| {
                            let _ = driver_clone;
                        },
                    ),
                )
                .unwrap(),
        );
        let always_on_dispatcher = dispatcher.always_on_dispatcher();
        driver.inner.lock().dispatcher = Some(dispatcher);
        driver.inner.lock().always_on_dispatcher = Some(always_on_dispatcher);
        *driver.runtime_handle.lock() = Some(driver_runtime_handle);
        driver
    }

    /// Creates a mock driver and starts it, returning the handles a test needs to drive it and
    /// observe its shutdown. The returned `Scope` must stay alive for the duration of the test.
    async fn start_test_driver(
        env: &fdf_env::Environment,
    ) -> (Arc<Driver>, fdh::DriverProxy, oneshot::Receiver<Weak<Driver>>, fuchsia_async::Scope)
    {
        start_test_driver_with_args(env, fidl_fdf::DriverStartArgs::default()).await
    }

    async fn start_test_driver_with_args(
        env: &fdf_env::Environment,
        start_args: fidl_fdf::DriverStartArgs,
    ) -> (Arc<Driver>, fdh::DriverProxy, oneshot::Receiver<Weak<Driver>>, fuchsia_async::Scope)
    {
        let scope = fuchsia_async::Scope::new();
        let driver = create_test_driver(env);
        let (driver_proxy, driver_server_end) =
            fidl::endpoints::create_proxy::<fdh::DriverMarker>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        driver.start(start_args, driver_server_end, shutdown_tx, &scope).await.unwrap();
        (driver, driver_proxy, shutdown_rx, scope)
    }

    #[fuchsia::test]
    async fn test_run_termination_watcher_forwards_health_check() {
        let env = fdf_env::Environment::start(0).unwrap();
        let (driver, driver_proxy, shutdown_rx, _scope) = start_test_driver(&env).await;

        let fast_res =
            driver_proxy.health_check(fidl_fdf::HealthCheckRequest::Fast).await.unwrap().unwrap();
        let conditions = fast_res.conditions.expect("conditions");
        assert_eq!(conditions.len(), 1);
        assert_eq!(conditions[0].status, Some(fidl_fdf::HealthStatus::Degraded));
        assert_eq!(conditions[0].reason_code.as_deref(), Some("HOST_FORWARD_OK"));
        assert_eq!(conditions[0].last_observed, Some(fidl::BootInstant::from_nanos(7)));

        let deep_res = driver_proxy.health_check(fidl_fdf::HealthCheckRequest::Deep).await.unwrap();
        assert_eq!(deep_res.unwrap_err(), fidl_fdf::HealthCheckError::NotSupported);

        driver_proxy.stop().unwrap();
        drop(driver);
        shutdown_rx.await.unwrap();
        env.destroy_all_dispatchers();
    }

    #[fuchsia::test]
    async fn test_run_termination_watcher_rejects_health_check_while_suspended() {
        SUSPEND_TEST_HEALTH_CHECK_CALLS.store(0, Ordering::SeqCst);
        let env = fdf_env::Environment::start(0).unwrap();
        let (driver, driver_proxy, shutdown_rx, _scope) = start_test_driver_with_args(
            &env,
            fidl_fdf::DriverStartArgs {
                url: Some("suspend-test".to_string()),
                ..Default::default()
            },
        )
        .await;

        for state in [
            DriverPowerState::SuspendRequested,
            DriverPowerState::Suspended,
            DriverPowerState::ResumeRequested,
        ] {
            driver.inner.lock().power_state = state;
            let res = driver_proxy.health_check(fidl_fdf::HealthCheckRequest::Fast).await.unwrap();
            assert_eq!(res.unwrap_err(), fidl_fdf::HealthCheckError::InvalidState);
            assert_eq!(SUSPEND_TEST_HEALTH_CHECK_CALLS.load(Ordering::SeqCst), 0);
        }

        driver.inner.lock().power_state = DriverPowerState::Resumed;
        let fast_res =
            driver_proxy.health_check(fidl_fdf::HealthCheckRequest::Fast).await.unwrap().unwrap();
        let conditions = fast_res.conditions.expect("conditions");
        assert_eq!(conditions.len(), 1);
        assert_eq!(conditions[0].reason_code.as_deref(), Some("HOST_FORWARD_OK"));
        assert_eq!(SUSPEND_TEST_HEALTH_CHECK_CALLS.load(Ordering::SeqCst), 1);

        driver_proxy.stop().unwrap();
        drop(driver);
        shutdown_rx.await.unwrap();
        env.destroy_all_dispatchers();
    }

    #[fuchsia::test]
    async fn test_run_termination_watcher_health_check_timeout_and_concurrent_stop() {
        let env = fdf_env::Environment::start(0).unwrap();
        let (driver, driver_proxy, shutdown_rx, _scope) = start_test_driver(&env).await;

        // Ping level hangs in MockDriverServer and must time out after 250ms in driver-host.
        let ping_res = driver_proxy.health_check(fidl_fdf::HealthCheckRequest::Ping).await.unwrap();
        assert_eq!(ping_res.unwrap_err(), fidl_fdf::HealthCheckError::Timeout);

        // Start another hanging health check and immediately issue Stop to verify Stop is not
        // blocked by an in-flight HealthCheck.
        let inflight_fut = driver_proxy.health_check(fidl_fdf::HealthCheckRequest::Ping);
        driver_proxy.stop().unwrap();
        drop(driver);
        shutdown_rx.await.unwrap();
        // The in-flight check is answered with INVALID_STATE, not cut short by the channel
        // closing, so driver_manager never sees a two-way call end in a ZX_OK epitaph.
        let inflight_res = inflight_fut.await.unwrap();
        assert_eq!(inflight_res.unwrap_err(), fidl_fdf::HealthCheckError::InvalidState);
        env.destroy_all_dispatchers();
    }

    #[fuchsia::test]
    async fn test_run_termination_watcher_in_flight_health_check_answers_invalid_state_on_suspend()
    {
        let env = fdf_env::Environment::start(0).unwrap();
        let (driver, driver_proxy, shutdown_rx, _scope) = start_test_driver(&env).await;

        // Ping level hangs in MockDriverServer. Forward it while resumed, then suspend the driver
        // while the check is in flight: when its 250ms deadline runs out, the completion must be
        // answered INVALID_STATE (the driver is not resumed) rather than TIMEOUT.
        let inflight_fut = driver_proxy.health_check(fidl_fdf::HealthCheckRequest::Ping);
        fuchsia_async::Timer::new(fuchsia_async::MonotonicInstant::after(
            zx::MonotonicDuration::from_millis(50),
        ))
        .await;
        driver.inner.lock().power_state = DriverPowerState::Suspended;
        let inflight_res = inflight_fut.await.unwrap();
        assert_eq!(inflight_res.unwrap_err(), fidl_fdf::HealthCheckError::InvalidState);

        driver.inner.lock().power_state = DriverPowerState::Resumed;
        driver_proxy.stop().unwrap();
        drop(driver);
        shutdown_rx.await.unwrap();
        env.destroy_all_dispatchers();
    }

    #[fuchsia::test]
    async fn test_run_termination_watcher_rejects_unknown_health_check_level() {
        let env = fdf_env::Environment::start(0).unwrap();
        let (driver, driver_proxy, shutdown_rx, _scope) = start_test_driver(&env).await;

        // A level this host does not know is answered locally rather than forwarded.
        let unknown = fidl_fdf::HealthCheckRequest::from_primitive_allow_unknown(0xdead);
        let res = driver_proxy.health_check(unknown).await.unwrap();
        assert_eq!(res.unwrap_err(), fidl_fdf::HealthCheckError::NotSupported);

        driver_proxy.stop().unwrap();
        drop(driver);
        shutdown_rx.await.unwrap();
        env.destroy_all_dispatchers();
    }

    #[fuchsia::test]
    async fn test_run_termination_watcher_forwards_on_health_alert() {
        let env = fdf_env::Environment::start(0).unwrap();
        let (driver, driver_proxy, shutdown_rx, _scope) = start_test_driver(&env).await;

        let mut event_stream = driver_proxy.take_event_stream();
        let _ =
            driver_proxy.health_check(fidl_fdf::HealthCheckRequest::Fast).await.unwrap().unwrap();

        let event = event_stream.try_next().await.unwrap().expect("expected OnHealthAlert event");
        let fdh::DriverEvent::OnHealthAlert { alert } = event;
        let fidl_fdf::HealthAlert::UpstreamNeighborFault(details) = alert else {
            panic!("expected UpstreamNeighborFault, got {alert:?}");
        };
        let conditions = details.conditions.expect("expected alert conditions");
        assert_eq!(conditions.len(), 1);
        assert_eq!(conditions[0].status, Some(fidl_fdf::HealthStatus::Degraded));
        assert_eq!(conditions[0].reason_code.as_deref(), Some("LINK_TIMEOUT"));
        assert_eq!(conditions[0].message.as_deref(), Some("neighbor link timed out"));

        driver_proxy.stop().unwrap();
        drop(driver);
        shutdown_rx.await.unwrap();
        env.destroy_all_dispatchers();
    }
}
