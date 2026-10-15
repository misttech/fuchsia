// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::node_remover::NodeRemover;
use async_trait::async_trait;
use fidl_fuchsia_diagnostics as fdiagnostics;
use fidl_fuchsia_kernel as fkernel;
use fidl_fuchsia_process_lifecycle as flifecycle;
use fidl_fuchsia_system_state as fsystem_state;
use fuchsia_async as fasync;
use fuchsia_component::client::{connect_to_protocol, connect_to_protocol_sync};
use fuchsia_component::server::{FidlService, ServiceFs, ServiceObjLocal};
use futures::channel::oneshot;
use futures::prelude::*;
use log::{error, info, warn};
use std::cell::RefCell;
use std::rc::Rc;
use zx::sys::{
    ZX_SYSTEM_POWERCTL_ACK_KERNEL_INITIATED_REBOOT, ZX_SYSTEM_POWERCTL_REBOOT,
    ZX_SYSTEM_POWERCTL_REBOOT_BOOTLOADER, ZX_SYSTEM_POWERCTL_REBOOT_RECOVERY,
    ZX_SYSTEM_POWERCTL_SHUTDOWN,
};

/// System power operations.
#[async_trait(?Send)]
pub trait SystemPower {
    /// Returns the current system power state.
    async fn get_system_power_state(&self) -> fsystem_state::SystemPowerState;

    /// Executes a system power control command (e.g. reboot, shutdown).
    fn system_powerctl(&self, cmd: u32) -> Result<(), zx::Status>;

    /// Returns true if the necessary power resource is available.
    fn has_resources(&self) -> bool;

    /// Exits the process.
    fn exit(&self, status: i32);
}

/// Real implementation of [`SystemPower`] that makes actual system calls.
pub struct RealSystemPower {
    power_resource: Option<zx::Resource>,
}

impl RealSystemPower {
    pub fn new(power_resource: Option<zx::Resource>) -> Self {
        Self { power_resource }
    }
}

#[async_trait(?Send)]
impl SystemPower for RealSystemPower {
    async fn get_system_power_state(&self) -> fsystem_state::SystemPowerState {
        get_system_power_state().await
    }

    fn system_powerctl(&self, cmd: u32) -> Result<(), zx::Status> {
        let Some(power_resource) = &self.power_resource else {
            return Err(zx::Status::INVALID_ARGS);
        };
        zx::Status::ok(unsafe {
            zx::sys::zx_system_powerctl(power_resource.raw_handle(), cmd, std::ptr::null())
        })
    }

    fn has_resources(&self) -> bool {
        self.power_resource.is_some()
    }

    fn exit(&self, status: i32) {
        std::process::exit(status);
    }
}

#[async_trait(?Send)]
impl<T: SystemPower + ?Sized> SystemPower for Rc<T> {
    async fn get_system_power_state(&self) -> fsystem_state::SystemPowerState {
        (**self).get_system_power_state().await
    }
    fn system_powerctl(&self, cmd: u32) -> Result<(), zx::Status> {
        (**self).system_powerctl(cmd)
    }
    fn has_resources(&self) -> bool {
        (**self).has_resources()
    }
    fn exit(&self, status: i32) {
        (**self).exit(status)
    }
}

#[derive(Copy, Clone, PartialEq, Debug)]
enum State {
    Running,
    PackageStopping,
    PackageStopped,
    BootStopping,
    Stopped,
}

type ShutdownSender = oneshot::Sender<Result<(), zx::Status>>;

struct LifecycleServer {
    on_stop: RefCell<Option<oneshot::Sender<ShutdownSender>>>,
}

impl LifecycleServer {
    fn new(on_stop: oneshot::Sender<ShutdownSender>) -> Self {
        Self { on_stop: RefCell::new(Some(on_stop)) }
    }

    async fn serve(
        self: Rc<Self>,
        mut stream: flifecycle::LifecycleRequestStream,
    ) -> Result<(), fidl::Error> {
        if let Some(request) = stream.try_next().await? {
            match request {
                flifecycle::LifecycleRequest::Stop { control_handle } => {
                    let (tx, rx) = oneshot::channel();
                    let on_stop = self.on_stop.borrow_mut().take();
                    if let Some(on_stop) = on_stop {
                        let _ = on_stop.send(tx);
                        if let Ok(result) = rx.await {
                            control_handle.shutdown_with_epitaph(result);
                        } else {
                            control_handle.shutdown_with_epitaph(zx::Status::INTERNAL);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

struct ShutdownManagerState {
    state: State,
    received_boot_shutdown_signal: bool,
    package_shutdown_complete_callbacks: Vec<ShutdownSender>,
    boot_shutdown_complete_callbacks: Vec<ShutdownSender>,
    lifecycle_stop: bool,
}

pub struct ShutdownManager<P: SystemPower + 'static = RealSystemPower> {
    node_remover: Rc<dyn NodeRemover>,
    log_flush: Option<fdiagnostics::LogFlusherProxy>,
    internal_state: RefCell<ShutdownManagerState>,
    scope: fasync::Scope,
    system_power: P,
}

fn get_power_resource() -> Result<zx::Resource, anyhow::Error> {
    let client = connect_to_protocol_sync::<fkernel::PowerResourceMarker>()?;
    let resource = client.get(zx::MonotonicInstant::INFINITE)?;
    Ok(resource)
}

async fn get_system_power_state() -> fsystem_state::SystemPowerState {
    let client = match connect_to_protocol::<fsystem_state::SystemStateTransitionMarker>() {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to connect to StateStateTransition: {}, falling back to default", e);
            return fsystem_state::SystemPowerState::Reboot;
        }
    };

    match client.get_termination_system_state().await {
        Ok(state) => state,
        Err(e) => {
            error!("Failed to get termination system state: {}, falling back to default", e);
            fsystem_state::SystemPowerState::Reboot
        }
    }
}

impl ShutdownManager<RealSystemPower> {
    /// Creates a [`ShutdownManager`] from the FIDL services available in the incoming namespace.
    pub fn from_incoming(node_remover: Rc<dyn NodeRemover>) -> Rc<Self> {
        let power_resource = get_power_resource()
            .inspect_err(|e| {
                info!("Failed to get power resource, assuming test environment: {}", e)
            })
            .ok();
        let log_flush = connect_to_protocol::<fdiagnostics::LogFlusherMarker>()
            .inspect_err(|e| error!("Failed to connect to LogFlusher: {}", e))
            .ok();
        Self::new(node_remover, RealSystemPower::new(power_resource), log_flush)
    }
}

impl<P: SystemPower + 'static> ShutdownManager<P> {
    pub fn new(
        node_remover: Rc<dyn NodeRemover>,
        system_power: P,
        log_flush: Option<fdiagnostics::LogFlusherProxy>,
    ) -> Rc<Self> {
        let shutdown_manager = Rc::new(Self {
            node_remover: node_remover.clone(),
            internal_state: RefCell::new(ShutdownManagerState {
                state: State::Running,
                received_boot_shutdown_signal: false,
                package_shutdown_complete_callbacks: Vec::new(),
                boot_shutdown_complete_callbacks: Vec::new(),
                lifecycle_stop: false,
            }),
            log_flush,
            scope: fasync::Scope::new_with_name("shutdown_manager"),
            system_power,
        });

        let weak_manager = Rc::downgrade(&shutdown_manager);
        node_remover.set_on_removal_timeout_callback(Box::new(move || {
            if let Some(strong_manager) = weak_manager.upgrade() {
                info!(
                    "Timed out waiting for nodes to be removed, issuing syscall to reboot/shutdown"
                );
                let strong_manager_clone = strong_manager.clone();
                strong_manager.scope.spawn_local(async move {
                    strong_manager_clone.execute_shutdown_strategy().await;
                });
            }
        }));

        shutdown_manager
    }

    pub fn publish<'a>(self: &Rc<Self>, fs: &mut ServiceFs<ServiceObjLocal<'a, ()>>) {
        let self_clone = self.clone();
        let (tx, rx) = oneshot::channel::<ShutdownSender>();
        self.scope.spawn_local(async move {
            if let Ok(sender) = rx.await {
                let status = self_clone.signal_package_shutdown().await;
                let _ = sender.send(status);
            }
        });
        let devfs_with_pkg_lifecycle = Rc::new(LifecycleServer::new(tx));

        let scope = self.scope.as_handle().clone();
        fs.dir("svc").add_service_at(
            "fuchsia.device.fs.with.pkg.lifecycle.Lifecycle",
            FidlService::from(move |stream: flifecycle::LifecycleRequestStream| {
                let devfs_with_pkg_lifecycle = devfs_with_pkg_lifecycle.clone();
                scope.spawn_local(async move {
                    devfs_with_pkg_lifecycle.serve(stream).await.unwrap_or_else(|e| {
                        error!("Failed to serve devfs with pkg lifecycle: {}", e)
                    });
                });
            }),
        );

        let self_clone = self.clone();
        let (tx, rx) = oneshot::channel::<ShutdownSender>();
        self.scope.spawn_local(async move {
            if let Ok(sender) = rx.await {
                let status = self_clone.signal_boot_shutdown().await;
                let _ = sender.send(status);
            }
        });
        let devfs_lifecycle = Rc::new(LifecycleServer::new(tx));

        let scope = self.scope.as_handle().clone();
        fs.dir("svc").add_service_at(
            "fuchsia.device.fs.lifecycle.Lifecycle",
            FidlService::from(move |stream: flifecycle::LifecycleRequestStream| {
                let devfs_lifecycle = devfs_lifecycle.clone();
                scope.spawn_local(async move {
                    devfs_lifecycle
                        .serve(stream)
                        .await
                        .unwrap_or_else(|e| error!("Failed to serve devfs lifecycle: {}", e));
                });
            }),
        );

        // Bind to process lifecycle
        let self_clone = self.clone();
        let (tx, rx) = oneshot::channel::<ShutdownSender>();
        self.scope.spawn_local(async move {
            if let Ok(sender) = rx.await {
                self_clone.internal_state.borrow_mut().lifecycle_stop = true;
                let status = self_clone.signal_boot_shutdown().await;
                let _ = sender.send(status);
            }
        });
        let lifecycle_server = Rc::new(LifecycleServer::new(tx));

        if let Some(handle) =
            fuchsia_runtime::take_startup_handle(fuchsia_runtime::HandleType::Lifecycle.into())
        {
            let channel = zx::Channel::from(handle);
            let server_end =
                fidl::endpoints::ServerEnd::<flifecycle::LifecycleMarker>::new(channel);
            let stream = server_end.into_stream();

            let self_clone = self.clone();
            self.scope.spawn_local(async move {
                if let Err(e) = lifecycle_server.serve(stream).await {
                    error!("Lifecycle connection got unbound: {}", e);
                    // Per C++ implementation, we should shut down if this happens.
                    let _ = self_clone.signal_boot_shutdown().await;
                }
            });
        } else {
            info!(concat!(
                "No valid handle found for lifecycle events, assuming test environment ",
                "and continuing"
            ));
        }
    }

    async fn on_package_shutdown_complete(&self) {
        info!("Package shutdown complete");
        let received_boot_shutdown_signal = {
            let mut internal_state = self.internal_state.borrow_mut();
            assert_eq!(internal_state.state, State::PackageStopping);
            internal_state.state = State::PackageStopped;

            for sender in internal_state.package_shutdown_complete_callbacks.drain(..) {
                let _ = sender.send(Ok(()));
            }

            if internal_state.received_boot_shutdown_signal {
                internal_state.state = State::BootStopping;
                true
            } else {
                false
            }
        };

        if received_boot_shutdown_signal {
            self.node_remover.shutdown_all_drivers().await;
            self.on_boot_shutdown_complete().await;
        }
    }

    async fn on_boot_shutdown_complete(&self) {
        {
            let mut internal_state = self.internal_state.borrow_mut();
            assert_eq!(internal_state.state, State::BootStopping);
            internal_state.state = State::Stopped;
        }
        self.execute_shutdown_strategy().await;
        let mut internal_state = self.internal_state.borrow_mut();
        for sender in internal_state.boot_shutdown_complete_callbacks.drain(..) {
            let _ = sender.send(Ok(()));
        }
    }

    async fn signal_package_shutdown(&self) -> Result<(), zx::Status> {
        // TODO: switch logs to debuglog

        // We explicitly drop this before going into the await.
        #![allow(clippy::await_holding_refcell_ref)]
        let mut internal_state = self.internal_state.borrow_mut();

        match internal_state.state {
            State::Running | State::PackageStopping => {
                let (tx, rx) = oneshot::channel();
                internal_state.package_shutdown_complete_callbacks.push(tx);
                if internal_state.state == State::Running {
                    internal_state.state = State::PackageStopping;
                    drop(internal_state);
                    self.node_remover.shutdown_pkg_drivers().await;
                    self.on_package_shutdown_complete().await;
                } else {
                    drop(internal_state);
                }
                rx.await.unwrap_or(Err(zx::Status::INTERNAL))
            }
            _ => Ok(()),
        }
    }

    async fn signal_boot_shutdown(&self) -> Result<(), zx::Status> {
        // We explicitly drop this before going into the await.
        #![allow(clippy::await_holding_refcell_ref)]
        let mut internal_state = self.internal_state.borrow_mut();

        if internal_state.state == State::Stopped {
            return Ok(());
        }

        let (tx, rx) = oneshot::channel();
        internal_state.boot_shutdown_complete_callbacks.push(tx);

        internal_state.received_boot_shutdown_signal = true;
        let state = internal_state.state;
        match state {
            State::Running | State::PackageStopped => {
                internal_state.state = State::BootStopping;
                drop(internal_state);

                self.node_remover.shutdown_all_drivers().await;
                self.on_boot_shutdown_complete().await;
            }
            State::BootStopping => {
                error!("SignalBootShutdown() called during shutdown.");
            }
            _ => {}
        }
        rx.await.unwrap_or(Err(zx::Status::INTERNAL))
    }

    async fn execute_shutdown_strategy(&self) {
        if !self.system_power.has_resources() {
            warn!("Invalid Power resource. Assuming test.");
            let internal_state = self.internal_state.borrow();
            if internal_state.lifecycle_stop {
                info!("Exiting driver manager gracefully");
                self.system_power.exit(0);
            }
            return;
        }

        let shutdown_system_state = self.system_power.get_system_power_state().await;
        info!("Suspend fallback with flags {:?}", shutdown_system_state);
        let what = "zx_system_powerctl";

        info!("Flushing logs.");
        if let Some(log_flush) = &self.log_flush
            && let Err(e) = log_flush.wait_until_flushed().await
        {
            warn!("Failed to flush logs: {}", e);
        }

        info!("Executing powerctl.");
        let status = match shutdown_system_state {
            fsystem_state::SystemPowerState::Reboot | fsystem_state::SystemPowerState::Mexec => {
                self.system_power.system_powerctl(ZX_SYSTEM_POWERCTL_REBOOT)
            }
            fsystem_state::SystemPowerState::RebootBootloader => {
                self.system_power.system_powerctl(ZX_SYSTEM_POWERCTL_REBOOT_BOOTLOADER)
            }
            fsystem_state::SystemPowerState::RebootRecovery => {
                self.system_power.system_powerctl(ZX_SYSTEM_POWERCTL_REBOOT_RECOVERY)
            }
            fsystem_state::SystemPowerState::RebootKernelInitiated => {
                let status = self
                    .system_power
                    .system_powerctl(ZX_SYSTEM_POWERCTL_ACK_KERNEL_INITIATED_REBOOT);
                if status.is_ok() {
                    // sleep indefinitely
                    loop {
                        fasync::Timer::new(std::time::Duration::from_secs(5 * 60)).await;
                        println!(
                            "driver_manager: unexpectedly still running after successful reboot syscall"
                        );
                    }
                }
                status
            }
            fsystem_state::SystemPowerState::Poweroff => {
                self.system_power.system_powerctl(ZX_SYSTEM_POWERCTL_SHUTDOWN)
            }
            fsystem_state::SystemPowerState::FullyOn
            | fsystem_state::SystemPowerState::SuspendRam => {
                error!("Unexpected shutdown state requested: {:?}", shutdown_system_state);
                Err(zx::Status::INVALID_ARGS)
            }
        };

        let internal_state = self.internal_state.borrow();
        if internal_state.lifecycle_stop {
            info!("Exiting driver manager gracefully");
            self.system_power.exit(0);
        }

        warn!("{}: {status:?}", what);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockNodeRemover {
        timeout_callback: RefCell<Option<Box<dyn Fn()>>>,
        shutdown_all_tx: RefCell<Option<oneshot::Sender<()>>>,
        received_shutdown_all_request_tx: RefCell<Option<oneshot::Sender<()>>>,
        received_shutdown_all_request_rx: RefCell<Option<oneshot::Receiver<()>>>,
    }

    impl MockNodeRemover {
        fn new() -> Self {
            let (tx, rx) = oneshot::channel();
            Self {
                timeout_callback: RefCell::new(None),
                shutdown_all_tx: RefCell::new(None),
                received_shutdown_all_request_tx: RefCell::new(Some(tx)),
                received_shutdown_all_request_rx: RefCell::new(Some(rx)),
            }
        }

        fn complete_shutdown_all(&self) {
            if let Some(tx) = self.shutdown_all_tx.borrow_mut().take() {
                let _ = tx.send(());
            }
        }

        fn timeout_node_removal(&self) {
            if let Some(cb) = self.timeout_callback.borrow().as_ref() {
                cb();
            }
        }

        async fn wait_for_shutdown_all_request(&self) {
            let rx = self.received_shutdown_all_request_rx.borrow_mut().take().unwrap();
            let _ = rx.await;
        }
    }

    #[async_trait(?Send)]
    impl NodeRemover for MockNodeRemover {
        async fn shutdown_all_drivers(&self) {
            if let Some(tx) = self.received_shutdown_all_request_tx.borrow_mut().take() {
                let _ = tx.send(());
            }
            let (tx, rx) = oneshot::channel();
            *self.shutdown_all_tx.borrow_mut() = Some(tx);
            let _ = rx.await;
        }
        async fn shutdown_pkg_drivers(&self) {}
        fn set_on_removal_timeout_callback(&self, callback: Box<dyn Fn()>) {
            *self.timeout_callback.borrow_mut() = Some(callback);
        }
    }

    struct MockSystemPower {
        state: fsystem_state::SystemPowerState,
        system_powerctl_called: RefCell<bool>,
        system_powerctl_cmd: RefCell<Option<u32>>,
        system_powerctl_tx: RefCell<Option<oneshot::Sender<()>>>,
        system_powerctl_rx: RefCell<Option<oneshot::Receiver<()>>>,
        exit_called: RefCell<bool>,
        exit_tx: RefCell<Option<oneshot::Sender<()>>>,
        exit_rx: RefCell<Option<oneshot::Receiver<()>>>,
    }

    impl MockSystemPower {
        fn new(state: fsystem_state::SystemPowerState) -> Self {
            let (p_tx, p_rx) = oneshot::channel();
            let (e_tx, e_rx) = oneshot::channel();
            Self {
                state,
                system_powerctl_called: RefCell::new(false),
                system_powerctl_cmd: RefCell::new(None),
                system_powerctl_tx: RefCell::new(Some(p_tx)),
                system_powerctl_rx: RefCell::new(Some(p_rx)),
                exit_called: RefCell::new(false),
                exit_tx: RefCell::new(Some(e_tx)),
                exit_rx: RefCell::new(Some(e_rx)),
            }
        }

        // Blocks until the shutdown manager calls `system_powerctl()`.
        async fn wait_for_system_powerctl(&self) {
            let rx = self.system_powerctl_rx.borrow_mut().take().unwrap();
            let _ = rx.await;
        }

        // Blocks until the shutdown manager calls `exit()`.
        async fn wait_for_exit(&self) {
            let rx = self.exit_rx.borrow_mut().take().unwrap();
            let _ = rx.await;
        }
    }

    #[async_trait(?Send)]
    impl SystemPower for MockSystemPower {
        async fn get_system_power_state(&self) -> fsystem_state::SystemPowerState {
            self.state
        }

        fn system_powerctl(&self, cmd: u32) -> Result<(), zx::Status> {
            *self.system_powerctl_called.borrow_mut() = true;
            *self.system_powerctl_cmd.borrow_mut() = Some(cmd);
            if let Some(tx) = self.system_powerctl_tx.borrow_mut().take() {
                let _ = tx.send(());
            }
            Ok(())
        }

        fn has_resources(&self) -> bool {
            true
        }

        fn exit(&self, _status: i32) {
            *self.exit_called.borrow_mut() = true;
            if let Some(tx) = self.exit_tx.borrow_mut().take() {
                let _ = tx.send(());
            }
        }
    }

    struct FakeLogFlusher {
        flushed: RefCell<bool>,
    }

    impl FakeLogFlusher {
        fn new() -> Self {
            Self { flushed: RefCell::new(false) }
        }
    }

    async fn serve_log_flusher(
        fake_log_flusher: Rc<FakeLogFlusher>,
        mut stream: fdiagnostics::LogFlusherRequestStream,
    ) -> Result<(), fidl::Error> {
        while let Some(request) = stream.try_next().await? {
            if let fdiagnostics::LogFlusherRequest::WaitUntilFlushed { responder } = request {
                *fake_log_flusher.flushed.borrow_mut() = true;
                responder.send()?;
            }
        }
        Ok(())
    }

    fn setup_log_flusher(
        scope: &fasync::Scope,
    ) -> (fdiagnostics::LogFlusherProxy, Rc<FakeLogFlusher>) {
        let (proxy, stream) = fidl::endpoints::create_proxy::<fdiagnostics::LogFlusherMarker>();
        let fake = Rc::new(FakeLogFlusher::new());
        let fake_clone = fake.clone();
        scope.spawn_local(async move {
            if let Err(e) = serve_log_flusher(fake_clone, stream.into_stream()).await {
                error!("Error serving LogFlusher: {}", e);
            }
        });
        (proxy, fake)
    }

    async fn run_shutdown_manager_test<F, Fut>(state: fsystem_state::SystemPowerState, test: F)
    where
        F: FnOnce(flifecycle::LifecycleProxy, Rc<MockNodeRemover>, Rc<MockSystemPower>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let node_remover = Rc::new(MockNodeRemover::new());
        let system_power = Rc::new(MockSystemPower::new(state));

        let scope = fasync::Scope::new();
        let (log_flush_proxy, fake_log_flusher) = setup_log_flusher(&scope);

        let shutdown_manager =
            ShutdownManager::new(node_remover.clone(), system_power.clone(), Some(log_flush_proxy));

        let (lifecycle_proxy, stream) =
            fidl::endpoints::create_proxy::<flifecycle::LifecycleMarker>();
        let (tx, rx) = oneshot::channel::<ShutdownSender>();

        let self_clone = shutdown_manager.clone();
        shutdown_manager.scope.spawn_local(async move {
            if let Ok(sender) = rx.await {
                self_clone.internal_state.borrow_mut().lifecycle_stop = true;
                let status = self_clone.signal_boot_shutdown().await;
                let _ = sender.send(status);
            }
        });
        let lifecycle_server = Rc::new(LifecycleServer::new(tx));
        shutdown_manager.scope.spawn_local(async move {
            if let Err(e) = lifecycle_server.serve(stream.into_stream()).await {
                error!("Error serving Lifecycle in test: {}", e);
            }
        });

        test(lifecycle_proxy, node_remover, system_power).await;

        assert!(*fake_log_flusher.flushed.borrow());
    }

    // Verifies that if the node remover times out then the system still reboots.
    #[fuchsia::test]
    async fn test_reboot_timeout() {
        run_shutdown_manager_test(
            fsystem_state::SystemPowerState::Reboot,
            |lifecycle_proxy, node_remover, system_power| async move {
                // Tell the shutdown manager to stop and shutdown all nodes.
                lifecycle_proxy.stop().unwrap();

                // Wait for the node remover to receive the request to shutdown all nodes.
                node_remover.wait_for_shutdown_all_request().await;

                // Triggers the event that the node remover has timed out waiting for nodes to be removed.
                node_remover.timeout_node_removal();

                // Wait for the shutdown manager to complete and call `system_powerctl()`.
                system_power.wait_for_system_powerctl().await;

                // Wait for the shutdown manager to exit.
                system_power.wait_for_exit().await;

                // Node remover timed out and so `system_powerctl()` should still be called.
                assert!(*system_power.system_powerctl_called.borrow());
                assert_eq!(
                    Some(ZX_SYSTEM_POWERCTL_REBOOT),
                    *system_power.system_powerctl_cmd.borrow()
                );
                assert!(*system_power.exit_called.borrow());
            },
        )
        .await;
    }

    // Verifies the shutdown manager correctly removes all nodes when stopping and reboots the
    // system when in the reboot power-system state.
    #[fuchsia::test]
    async fn test_reboot_normal() {
        run_shutdown_manager_test(
            fsystem_state::SystemPowerState::Reboot,
            |lifecycle_proxy, node_remover, system_power| async move {
                // Tell the shutdown manager to stop and shutdown all nodes.
                lifecycle_proxy.stop().unwrap();

                // Wait for the node remover to receive the request to shutdown all nodes.
                node_remover.wait_for_shutdown_all_request().await;

                // Complete the node-removal request.
                node_remover.complete_shutdown_all();

                // Wait for the shutdown manager to complete and call `system_powerctl()`.
                system_power.wait_for_system_powerctl().await;

                // Wait for the shutdown manager to exit.
                system_power.wait_for_exit().await;

                // The shutdown manager should call `system_powerctl(ZX_SYSTEM_POWERCTL_REBOOT)`.
                assert!(*system_power.system_powerctl_called.borrow());
                assert_eq!(
                    Some(ZX_SYSTEM_POWERCTL_REBOOT),
                    *system_power.system_powerctl_cmd.borrow()
                );
                assert!(*system_power.exit_called.borrow());
            },
        )
        .await;
    }
}
