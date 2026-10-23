// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::ffi::c_void;
use core::ptr::NonNull;
use std::num::NonZero;
use std::sync::{Arc, OnceLock};

use log::{debug, error, warn};
use zx::Status;

use fdf::{
    AsAsyncDispatcherRef, AsyncDispatcher, Channel, DispatcherBuilder, DriverDispatcherRef,
    DriverHandle, fdf_handle_t,
};
use fdf_fidl::DriverChannel;
use fidl_next::{Request, Responder, Server, ServerDispatcher, ServerEnd};
use fidl_next_fuchsia_driver_framework::{
    Driver as DriverProtocol, DriverLocalServerHandler, DriverStartArgs, HealthCheckError, driver,
};

use crate::health::{HealthAlertReporter, run_health_check};
use crate::{Driver, DriverContext, DriverError};
use fdf_sys::fdf_dispatcher_get_current_dispatcher;
use fuchsia_async::LocalExecutorBuilder;

/// The driver transport the lifecycle server reads on. Reads are registered against the root
/// dispatcher's always-on interface so that requests (notably `Resume`) keep being delivered
/// while the driver's dispatchers are suspended.
pub(crate) type Transport = DriverChannel<AsyncDispatcher>;

/// Implements the lifecycle management of a rust driver, including starting and stopping it
/// and setting up the rust async dispatcher and logging for the driver to use, and running a
/// message loop for the driver start and stop messages.
///
/// Lifecycle requests are handled in order. Health checks are the one exception: they are
/// spawned as tasks on the same thread so that a slow or hung check can never delay `Stop`,
/// `Suspend` or `Resume`.
pub struct DriverServer<T> {
    /// Holds the lifecycle channel until [`Self::message_loop`] hands it to the fidl_next server.
    server_handle: OnceLock<Channel<[fidl_next::Chunk]>>,
    root_dispatcher: DriverDispatcherRef<'static>,
    /// The running driver, wrapped in [`Arc`] so health check tasks spawned on the local
    /// scope can hold a reference to it without `unsafe` lifetime extension. The health scope
    /// is cancelled before the driver is taken out of here to be stopped or dropped, so
    /// [`Driver::stop`] never runs concurrently with a health check.
    driver: OnceLock<Arc<T>>,
}

impl<T: Driver> DriverServer<T> {
    /// Called by the driver host to start the driver.
    ///
    /// # Safety
    ///
    /// The caller must provide a valid non-zero driver transport channel handle for
    /// `server_handle`.
    pub unsafe extern "C" fn initialize(server_handle: fdf_handle_t) -> *mut c_void {
        // SAFETY: We verify that the pointer returned is non-null, ensuring that this was
        // called from within a driver context.
        let root_dispatcher = NonNull::new(unsafe { fdf_dispatcher_get_current_dispatcher() })
            .expect("Non-null current dispatcher");
        // SAFETY: We use NonZero::new to verify that we've been given a non-zero
        // driver handle, and expect that the caller (which is the driver runtime) has given us
        // a valid driver transport fidl channel.
        let server_handle = OnceLock::from(unsafe {
            Channel::from_driver_handle(DriverHandle::new_unchecked(
                NonZero::new(server_handle).expect("valid driver handle"),
            ))
        });

        // SAFETY: the root dispatcher is expected to live as long as this driver is loaded.
        let root_dispatcher = unsafe { DriverDispatcherRef::from_raw(root_dispatcher) };
        // We leak the box holding the server so that the driver runtime can take control over the
        // lifetime of the server object.
        let server_ptr = Box::into_raw(Box::new(Self {
            server_handle,
            root_dispatcher: root_dispatcher.clone(),
            driver: OnceLock::default(),
        }));

        // Reconstitute the pointer to the `DriverServer` as a mut reference to use it in the main
        // loop.
        // SAFETY: We are the exclusive owner of the object until we drop the server handle,
        // triggering the driver host to call `destroy`.
        let server = unsafe { &mut *server_ptr };

        let root_dispatcher_always_on = root_dispatcher.always_on_dispatcher();
        server.start_rust_async_dispatcher(root_dispatcher_always_on);

        // Take the pointer of the server object to use as the identifier for the server to the
        // driver runtime. It uses this as an opaque identifier and expects no particular layout of
        // the object pointed to, and we use it to free the box at unload in `Self::destroy`.
        server_ptr.cast()
    }

    fn start_rust_async_dispatcher(
        &'static mut self,
        read_dispatcher: DriverDispatcherRef<'static>,
    ) {
        // Build a new dispatcher that we can have spin on a fuchsia-async executor main loop
        // to act as a reactor for non-driver events. Use the always_on_dispatcher on it because
        // this thread is always running and we don't want to hold up the driver's dispatcher
        // suspension operation.
        let rust_async_dispatcher = DispatcherBuilder::new()
            .name("fuchsia-async")
            .allow_thread_blocking()
            .scheduler_role(&self.root_dispatcher.scheduler_role())
            .create_released()
            .expect("failure creating blocking dispatcher for rust async")
            .always_on_dispatcher();
        // Post the task to the dispatcher that will run the fuchsia-async loop, and have it run
        // the server's message loop waiting for start and stop messages from the driver host.
        rust_async_dispatcher
            .post_task_sync(move |status| {
                // bail immediately if we were somehow cancelled before we started
                if status.is_ok() {
                    self.run_executor_loop(read_dispatcher);
                } else {
                    self.server_handle.take();
                }
            })
            .expect("failure spawning main event loop for rust async dispatch");
    }

    fn run_executor_loop(&'static mut self, read_dispatcher: DriverDispatcherRef<'static>) {
        fdf_core::override_current_dispatcher(self.root_dispatcher.clone(), || {
            // create and run a fuchsia-async executor, giving it the "root" dispatcher to
            // actually execute driver tasks on, as this thread will be effectively blocked
            // by the reactor loop.
            let port = zx::Port::create_with_opts(zx::PortOptions::BIND_TO_INTERRUPT);
            let mut executor = LocalExecutorBuilder::new().port(port).build();
            executor.run_singlethreaded(async move {
                self.message_loop(read_dispatcher).await;
            });
        });
    }

    /// Called by the driver host after shutting down a driver and once the handle passed to
    /// [`Self::initialize`] is dropped.
    ///
    /// # Safety
    ///
    /// This must only be called after the handle provided to [`Self::initialize`] has been
    /// dropped, which indicates that the main event loop of the driver lifecycle has ended.
    pub unsafe extern "C" fn destroy(obj: *mut c_void) {
        let obj: *mut Self = obj.cast();
        // SAFETY: We built this object in `initialize` and gave ownership of its
        // lifetime to the driver framework, which is now giving it to us to free.
        unsafe { drop(Box::from_raw(obj)) }
    }

    /// Implements the main message loop for handling start and stop messages from rust
    /// driver host and passing them on to the implementation of [`Driver`] we contain.
    async fn message_loop(&mut self, dispatcher: DriverDispatcherRef<'_>) {
        let server_handle = self.server_handle.take().expect("driver server handle already taken");
        let server_end: ServerEnd<DriverProtocol, Transport> = ServerEnd::from_untyped(
            DriverChannel::new_with_dispatcher(AsyncDispatcher::new(&dispatcher), server_handle),
        );
        let server = ServerDispatcher::new(server_end);
        let fidl_server = server.server();
        let handler = LifecycleHandler {
            server: Some(self),
            health_alert_reporter: HealthAlertReporter::new(fidl_server.clone()),
            fidl_server,
            health_tasks: Some(fuchsia_async::Scope::new_with_name("driver-health")),
        };
        match server.run_local(handler).await {
            // The driver host closed the channel without sending `Stop`.
            Ok(mut handler) if handler.server.is_some() => {
                warn!(
                    "Driver server channel closed before a stop message; \
                     stop() will not be called."
                );
                handler.finish().await;
            }
            Ok(_) => {}
            Err(e) => error!("Driver server channel failed: {e}, exiting main loop."),
        }
    }

    /// Handles the start message by initializing logging and calling the [`Driver::start`] with
    /// a constructed [`DriverContext`].
    ///
    /// # Panics
    ///
    /// This method panics if the start message has already been received.
    /// [`LifecycleHandler::start`] rejects a second start before reaching this method.
    async fn handle_start(
        &self,
        start_args: DriverStartArgs,
        health_alert_reporter: HealthAlertReporter,
    ) -> Result<(), Status> {
        let context = DriverContext::new(
            AsyncDispatcher::new(&self.root_dispatcher),
            start_args,
            health_alert_reporter,
        )?;
        context.start_logging(T::NAME)?;

        log::debug!("driver starting");

        let driver = T::start(context).await.map_err(DriverError::log_to_status)?;
        self.driver
            .set(Arc::new(driver))
            .map_err(|_| ())
            .expect("Driver received start message twice");
        Ok(())
    }

    async fn handle_stop(&mut self) {
        log::debug!("driver stopping");
        match self.driver.take() {
            Some(driver) => driver.stop().await,
            None => warn!("received stop message without a running driver"),
        }
    }

    async fn handle_suspend(&self) -> Result<(), Status> {
        log::debug!("driver suspending");
        let driver = self.driver.get().ok_or(Status::BAD_STATE)?;
        driver.system_suspend().await.map_err(DriverError::log_to_status)
    }

    async fn handle_resume(&self, lease: Option<zx::EventPair>) -> Result<(), Status> {
        log::debug!("driver resuming");
        let driver = self.driver.get().ok_or(Status::BAD_STATE)?;
        driver.system_resume(lease).await.map_err(DriverError::log_to_status)
    }

    /// Returns a reference to the driver, if it has been started.
    /// This is meant to be used only for testing.
    pub(crate) fn testing_get_driver(&self) -> Option<&'_ T> {
        self.driver.get().map(Arc::as_ref)
    }
}

/// The `fidl_next` server handler for the driver lifecycle protocol. It borrows the
/// [`DriverServer`] for the duration of [`DriverServer::message_loop`] only.
struct LifecycleHandler<'a, T> {
    /// `None` once the driver has stopped or failed to start, after which the server must not be
    /// touched: the driver host is free to destroy it as soon as the channel closes.
    server: Option<&'a mut DriverServer<T>>,
    fidl_server: Server<DriverProtocol, Transport>,
    /// The reporter handed to the driver through its [`DriverContext`].
    health_alert_reporter: HealthAlertReporter,
    /// Owns the in-flight health checks. Cancelled before the driver stops, so health checks
    /// never run concurrently with [`Driver::stop`] or outlive the driver. `None` afterwards.
    health_tasks: Option<fuchsia_async::Scope>,
}

impl<T: Driver> LifecycleHandler<'_, T> {
    /// Cancels all in-flight health checks and waits for them to finish.
    async fn cancel_health_tasks(&mut self) {
        if let Some(health_tasks) = self.health_tasks.take() {
            health_tasks.cancel().await;
        }
    }

    /// Tears down the driver and closes the channel. This is the last thing the handler does
    /// with the server: the driver host treats the channel closing as the signal that the driver
    /// is fully stopped.
    async fn finish(&mut self) {
        self.health_alert_reporter.close();
        self.cancel_health_tasks().await;
        if let Some(server) = self.server.take() {
            // The driver is dropped here, on the fuchsia-async thread and inside its executor,
            // so that it can release any async resources it holds.
            drop(server.driver.take());
        }
        self.fidl_server.close();
    }
}

fn log_reply<E: std::fmt::Display>(method: &str, reply: Result<(), E>) {
    if let Err(e) = reply {
        warn!("Failed to reply to {method}: {e}");
    }
}

impl<T: Driver> DriverLocalServerHandler<Transport> for LifecycleHandler<'_, T> {
    async fn start(
        &mut self,
        request: Request<driver::Start, Transport>,
        responder: Responder<driver::Start, Transport>,
    ) {
        let Some(server) = self.server.as_deref() else {
            return;
        };
        // A second start is rejected without disturbing the running driver. It must not go
        // through `handle_start`, whose failure closes the channel below.
        if server.driver.get().is_some() {
            error!("Driver received start message twice");
            log_reply("Start", responder.respond_err(Status::BAD_STATE).await);
            return;
        }
        let res = server
            .handle_start(request.payload().start_args, self.health_alert_reporter.clone())
            .await;
        let reply = match res {
            Ok(()) => responder.respond(()).await,
            Err(status) => responder.respond_err(status).await,
        };
        if let Err(e) = reply {
            error!("Failed to reply to Start: {e}, exiting main loop.");
            self.finish().await;
        } else if res.is_err() {
            debug!("driver failed to start, exiting main loop");
            self.finish().await;
        }
    }

    async fn stop(&mut self) {
        // Stop accepting health alerts and abandon any health checks still in flight before
        // calling `Driver::stop`.
        self.health_alert_reporter.close();
        self.cancel_health_tasks().await;
        if let Some(server) = self.server.as_deref_mut() {
            server.handle_stop().await;
        }
        self.finish().await;
    }

    async fn suspend(&mut self, responder: Responder<driver::Suspend, Transport>) {
        let Some(server) = self.server.as_deref() else {
            return;
        };
        let reply = match server.handle_suspend().await {
            Ok(()) => responder.respond(()).await,
            Err(status) => responder.respond_err(status).await,
        };
        log_reply("Suspend", reply);
    }

    async fn resume(
        &mut self,
        request: Request<driver::Resume, Transport>,
        responder: Responder<driver::Resume, Transport>,
    ) {
        let Some(server) = self.server.as_deref() else {
            return;
        };
        let lease = request.payload().power_element_lease;
        let reply = match server.handle_resume(lease).await {
            Ok(()) => responder.respond(()).await,
            Err(status) => responder.respond_err(status).await,
        };
        log_reply("Resume", reply);
    }

    async fn health_check(
        &mut self,
        request: Request<driver::HealthCheck, Transport>,
        responder: Responder<driver::HealthCheck, Transport>,
    ) {
        let request = request.payload().request;
        let running =
            self.server.as_deref().and_then(|server| Some((server, server.driver.get()?)));
        let (Some((server, driver)), Some(health_tasks)) = (running, &self.health_tasks) else {
            if let Err(e) = responder.respond_err(HealthCheckError::InvalidState).await {
                debug!("Failed to reply to HealthCheck: {e}");
            }
            return;
        };
        let driver = Arc::clone(driver);
        let root_dispatcher = server.root_dispatcher.clone();
        // Run the check as its own task so that it cannot hold up the requests that follow it,
        // including `Stop`. The task owns the responder and replies when the check completes.
        health_tasks.spawn_local(async move {
            let result = run_health_check(driver.as_ref(), root_dispatcher, request).await;
            // Unlike Start/Suspend/Resume, whose replies the host always waits for, a health
            // check reply can legitimately arrive after the host has given up and closed the
            // channel (the server's deadline is deliberately later than the host's), so a failed
            // reply is not a bug and must not panic.
            let reply = match result {
                Ok(response) => responder.respond(response).await,
                Err(e) => responder.respond_err(e).await,
            };
            if let Err(e) = reply {
                debug!("Failed to reply to HealthCheck: {e}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        DriverError, HealthAlert, HealthAlertReporter, HealthCheckRequest, HealthCheckResponse,
        HealthCheckResult, HealthCondition, HealthDiagnosticItem, HealthDiagnosticValue,
        HealthStatus,
    };

    use fdf::{CurrentDispatcher, OnDispatcher};
    use fdf_env::test::spawn_in_driver;
    use fidl_next_fuchsia_driver_framework::DriverClientHandler;
    use futures::StreamExt;
    use futures::channel::{mpsc, oneshot};
    use zx::Status;

    use fdf::Channel;
    use fidl_next::{ClientDispatcher, ClientEnd};

    #[derive(Default)]
    struct TestDriver {
        _not_empty: bool,
    }

    impl Driver for TestDriver {
        const NAME: &str = "test_driver";

        async fn start(context: DriverContext) -> Result<Self, DriverError> {
            let DriverContext { root_dispatcher, start_args, .. } = context;
            println!("created new test driver on dispatcher: {root_dispatcher:?}");
            println!("driver start message: {start_args:?}");
            Ok(Self::default())
        }
        async fn stop(&self) {
            println!("driver stop message");
        }
        async fn system_suspend(&self) -> Result<(), DriverError> {
            println!("driver suspend message");
            Ok(())
        }
        async fn system_resume(&self, _lease: Option<zx::EventPair>) -> Result<(), DriverError> {
            println!("driver resume message");
            Ok(())
        }
    }

    crate::driver_register!(TestDriver);

    #[derive(Debug)]
    struct DriverClient;
    impl DriverClientHandler for DriverClient {
        async fn on_health_alert(
            &mut self,
            _request: fidl_next::Request<
                fidl_next_fuchsia_driver_framework::driver::OnHealthAlert,
                fdf_fidl::DriverChannel,
            >,
        ) {
        }
    }

    #[test]
    fn register_driver() {
        assert_eq!(__fuchsia_driver_registration__.version, 1);
        let initialize_func = __fuchsia_driver_registration__.v1.initialize.expect("initializer");
        let destroy_func = __fuchsia_driver_registration__.v1.destroy.expect("destroy function");

        let (server_chan, client_chan) = Channel::<[fidl_next::Chunk]>::create();

        spawn_in_driver("driver registration", async move {
            let client_end: ClientEnd<fidl_next_fuchsia_driver_framework::Driver, _> =
                ClientEnd::from_untyped(fdf_fidl::DriverChannel::new(client_chan));
            let dispatcher = ClientDispatcher::new(client_end);
            let client = dispatcher.client();

            let client_task = CurrentDispatcher.spawn(async move {
                dispatcher.run(DriverClient).await.unwrap_err();
            });

            let channel_handle = server_chan.into_driver_handle().into_raw().get();
            let driver_server = unsafe { initialize_func(channel_handle) } as usize;
            assert_ne!(driver_server, 0);

            assert_eq!(client.suspend().await.unwrap(), Err(Status::BAD_STATE));
            assert_eq!(client.resume(None::<zx::EventPair>).await.unwrap(), Err(Status::BAD_STATE));

            client
                .start(fidl_next_fuchsia_driver_framework::DriverStartArgs::default())
                .await
                .unwrap()
                .unwrap();

            assert_eq!(
                client.start(DriverStartArgs::default()).await.unwrap(),
                Err(Status::BAD_STATE)
            );

            client.suspend().await.unwrap().unwrap();
            client.resume(None::<zx::EventPair>).await.unwrap().unwrap();

            client.stop().await.unwrap();
            client_task.await.unwrap();

            unsafe {
                destroy_func(driver_server as *mut c_void);
            }
        })
    }

    struct TestDriverAnyhowSuccess;

    impl Driver for TestDriverAnyhowSuccess {
        const NAME: &str = "test_driver_anyhow_success";

        async fn start(_context: DriverContext) -> Result<Self, DriverError> {
            Ok(Self)
        }
        async fn stop(&self) {}
    }

    #[test]
    fn test_anyhow_success() {
        let registration = crate::macros::make_driver_registration::<TestDriverAnyhowSuccess>();
        let initialize_func = registration.v1.initialize.expect("initializer");
        let destroy_func = registration.v1.destroy.expect("destroy function");

        let (server_chan, client_chan) = Channel::<[fidl_next::Chunk]>::create();

        spawn_in_driver("driver anyhow success", async move {
            let client_end: ClientEnd<fidl_next_fuchsia_driver_framework::Driver, _> =
                ClientEnd::from_untyped(fdf_fidl::DriverChannel::new(client_chan));
            let dispatcher = ClientDispatcher::new(client_end);
            let client = dispatcher.client();

            let client_task = CurrentDispatcher.spawn(async move {
                dispatcher.run(DriverClient).await.unwrap_err();
            });

            let channel_handle = server_chan.into_driver_handle().into_raw().get();
            let driver_server = unsafe { initialize_func(channel_handle) } as usize;
            assert_ne!(driver_server, 0);

            client
                .start(fidl_next_fuchsia_driver_framework::DriverStartArgs::default())
                .await
                .unwrap()
                .unwrap();

            client.stop().await.unwrap();
            client_task.await.unwrap();

            unsafe {
                destroy_func(driver_server as *mut c_void);
            }
        })
    }

    struct TestDriverAnyhowFailure;

    impl Driver for TestDriverAnyhowFailure {
        const NAME: &str = "test_driver_anyhow_failure";

        async fn start(_context: DriverContext) -> Result<Self, DriverError> {
            Err(anyhow::anyhow!(Status::INVALID_ARGS).into())
        }
        async fn stop(&self) {}
    }

    #[test]
    fn test_anyhow_failure() {
        let registration = crate::macros::make_driver_registration::<TestDriverAnyhowFailure>();
        let initialize_func = registration.v1.initialize.expect("initializer");
        let destroy_func = registration.v1.destroy.expect("destroy function");

        let (server_chan, client_chan) = Channel::<[fidl_next::Chunk]>::create();

        spawn_in_driver("driver anyhow failure", async move {
            let client_end: ClientEnd<fidl_next_fuchsia_driver_framework::Driver, _> =
                ClientEnd::from_untyped(fdf_fidl::DriverChannel::new(client_chan));
            let dispatcher = ClientDispatcher::new(client_end);
            let client = dispatcher.client();

            let client_task = CurrentDispatcher.spawn(async move {
                dispatcher.run(DriverClient).await.unwrap_err();
            });

            let channel_handle = server_chan.into_driver_handle().into_raw().get();
            let driver_server = unsafe { initialize_func(channel_handle) } as usize;
            assert_ne!(driver_server, 0);

            let res = client
                .start(fidl_next_fuchsia_driver_framework::DriverStartArgs::default())
                .await
                .unwrap();

            assert_eq!(res.unwrap_err(), Status::INVALID_ARGS);

            client_task.await.unwrap();

            unsafe {
                destroy_func(driver_server as *mut c_void);
            }
        })
    }

    struct TestDriverAnyhowFailureDefault;

    impl Driver for TestDriverAnyhowFailureDefault {
        const NAME: &str = "test_driver_anyhow_failure_default";

        async fn start(_context: DriverContext) -> Result<Self, DriverError> {
            Err(anyhow::anyhow!("some generic error").into())
        }
        async fn stop(&self) {}
    }

    #[test]
    fn test_anyhow_failure_default() {
        let registration =
            crate::macros::make_driver_registration::<TestDriverAnyhowFailureDefault>();
        let initialize_func = registration.v1.initialize.expect("initializer");
        let destroy_func = registration.v1.destroy.expect("destroy function");

        let (server_chan, client_chan) = Channel::<[fidl_next::Chunk]>::create();

        spawn_in_driver("driver anyhow failure default", async move {
            let client_end: ClientEnd<fidl_next_fuchsia_driver_framework::Driver, _> =
                ClientEnd::from_untyped(fdf_fidl::DriverChannel::new(client_chan));
            let dispatcher = ClientDispatcher::new(client_end);
            let client = dispatcher.client();

            let client_task = CurrentDispatcher.spawn(async move {
                dispatcher.run(DriverClient).await.unwrap_err();
            });

            let channel_handle = server_chan.into_driver_handle().into_raw().get();
            let driver_server = unsafe { initialize_func(channel_handle) } as usize;
            assert_ne!(driver_server, 0);

            let res = client
                .start(fidl_next_fuchsia_driver_framework::DriverStartArgs::default())
                .await
                .unwrap();

            assert_eq!(res.unwrap_err(), Status::INTERNAL);

            client_task.await.unwrap();

            unsafe {
                destroy_func(driver_server as *mut c_void);
            }
        })
    }

    type TestClient = fidl_next::Client<DriverProtocol, DriverChannel>;
    type AlertReceiver = mpsc::UnboundedReceiver<HealthAlert>;

    /// Collects `OnHealthAlert` events sent by the server.
    #[derive(Debug)]
    struct AlertCollector(mpsc::UnboundedSender<HealthAlert>);

    impl DriverClientHandler for AlertCollector {
        async fn on_health_alert(
            &mut self,
            request: Request<driver::OnHealthAlert, DriverChannel>,
        ) {
            let _ = self.0.unbounded_send(request.payload().alert);
        }
    }

    /// Initializes a `DriverServer` for `D` over a fresh driver channel, runs `body` against a
    /// client of it, then waits for the server to release the channel and destroys it. `body` is
    /// responsible for stopping the driver if it started it. `body` also receives the stream of
    /// `OnHealthAlert` events the server sends.
    fn with_driver_server<D, F, Fut>(name: &str, body: F)
    where
        D: Driver,
        F: FnOnce(TestClient, AlertReceiver) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let registration = crate::macros::make_driver_registration::<D>();
        let initialize_func = registration.v1.initialize.expect("initializer");
        let destroy_func = registration.v1.destroy.expect("destroy function");
        let (server_chan, client_chan) = Channel::<[fidl_next::Chunk]>::create();

        spawn_in_driver(name, async move {
            let client_end: ClientEnd<DriverProtocol, _> =
                ClientEnd::from_untyped(DriverChannel::new(client_chan));
            let dispatcher = ClientDispatcher::new(client_end);
            let client = dispatcher.client();
            // Dropping the last `Client` stops the client dispatcher, so keep one alive until the
            // server has closed the channel regardless of what `body` does with its copy.
            let keepalive = client.clone();
            let (alert_tx, alert_rx) = mpsc::unbounded();

            let client_task = CurrentDispatcher.spawn(async move {
                // The dispatcher exits with `PeerClosed` once the server releases its end of the
                // channel, which is when the driver has finished stopping (or failed to start).
                dispatcher.run(AlertCollector(alert_tx)).await.unwrap_err();
            });

            let channel_handle = server_chan.into_driver_handle().into_raw().get();
            // SAFETY: `channel_handle` is a valid, non-zero FDF channel handle created above.
            // Ownership of the handle is given to `initialize_func`.
            let driver_server = unsafe { initialize_func(channel_handle) } as usize;
            assert_ne!(driver_server, 0);

            body(client, alert_rx).await;

            client_task.await.unwrap();
            drop(keepalive);
            // SAFETY: `driver_server` was returned by `initialize_func` and the server has
            // released its channel, so it is idle and safe to destroy.
            unsafe {
                destroy_func(driver_server as *mut c_void);
            }
        })
    }

    #[test]
    fn test_stop_before_start() {
        with_driver_server::<TestDriver, _, _>("stop before start", |client, _alerts| async move {
            client.stop().await.unwrap();
        });
    }

    #[test]
    fn test_requests_queued_behind_start() {
        with_driver_server::<TestDriver, _, _>(
            "requests queued behind start",
            |client, _alerts| async move {
                let start = client.start(DriverStartArgs::default());
                let suspend = client.suspend();
                let resume = client.resume(None::<zx::EventPair>);
                let stop = client.stop();
                let (start, suspend, resume, stop) = futures::join!(start, suspend, resume, stop);
                start.unwrap().unwrap();
                suspend.unwrap().unwrap();
                resume.unwrap().unwrap();
                stop.unwrap();
            },
        );
    }

    #[test]
    fn test_health_check_before_start_is_rejected() {
        with_driver_server::<TestDriver, _, _>(
            "health check before start",
            |client, _alerts| async move {
                assert_eq!(
                    client.health_check(HealthCheckRequest::Ping).await.unwrap(),
                    Err(HealthCheckError::InvalidState)
                );
                client.start(DriverStartArgs::default()).await.unwrap().unwrap();
                client.stop().await.unwrap();
            },
        );
    }

    #[test]
    fn test_health_check_default() {
        with_driver_server::<TestDriver, _, _>(
            "health check default",
            |client, _alerts| async move {
                client.start(DriverStartArgs::default()).await.unwrap().unwrap();

                // With the default `health_check`, PING answers OK (an empty response) after the
                // root dispatcher liveness check, and deeper levels are not supported. None of them
                // disturb the lifecycle requests that follow.
                let ping = client.health_check(HealthCheckRequest::Ping).await.unwrap();
                assert_eq!(ping, Ok(HealthCheckResponse::default()));
                for level in [HealthCheckRequest::Fast, HealthCheckRequest::Deep] {
                    let res = client.health_check(level).await.unwrap();
                    assert_eq!(res, Err(HealthCheckError::NotSupported));
                }
                client.suspend().await.unwrap().unwrap();
                client.stop().await.unwrap();
            },
        );
    }

    /// Overrides `health_check` to answer with structured diagnostics.
    struct CustomHealthCheckDriver;

    impl Driver for CustomHealthCheckDriver {
        const NAME: &str = "custom_health_check_driver";

        async fn start(_context: DriverContext) -> Result<Self, DriverError> {
            Ok(Self)
        }
        async fn stop(&self) {}

        async fn health_check(&self, request: HealthCheckRequest) -> HealthCheckResult {
            if request == HealthCheckRequest::Deep {
                return Err(HealthCheckError::NotSupported);
            }
            Ok(HealthCheckResponse {
                conditions: Some(vec![HealthCondition {
                    status: Some(HealthStatus::Degraded),
                    reason_code: Some("FIFO_UNDERRUN".to_string()),
                    message: Some("Recovered from underrun".to_string()),
                    ..Default::default()
                }]),
                diagnostics: Some(vec![HealthDiagnosticItem {
                    key: "underruns".to_string(),
                    value: HealthDiagnosticValue::UintValue(2),
                }]),
            })
        }
    }

    #[test]
    fn test_health_check_custom() {
        with_driver_server::<CustomHealthCheckDriver, _, _>(
            "health check custom",
            |client, _alerts| async move {
                client.start(DriverStartArgs::default()).await.unwrap().unwrap();

                let res = client.health_check(HealthCheckRequest::Fast).await.unwrap().unwrap();
                let conditions = res.conditions.as_ref().expect("conditions");
                assert_eq!(conditions.len(), 1);
                assert_eq!(conditions[0].status, Some(HealthStatus::Degraded));
                assert_eq!(conditions[0].reason_code.as_deref(), Some("FIFO_UNDERRUN"));
                assert_eq!(res.diagnostics.as_ref().map(|d| d.len()), Some(1));

                let err = client.health_check(HealthCheckRequest::Deep).await.unwrap();
                assert_eq!(err, Err(HealthCheckError::NotSupported));
                let unknown =
                    client.health_check(HealthCheckRequest::UnknownOrdinal_(99)).await.unwrap();
                assert_eq!(unknown, Err(HealthCheckError::NotSupported));

                client.stop().await.unwrap();
            },
        );
    }

    /// Signals `test_stop_with_hung_health_check` once the hung check has been entered. Only
    /// that test's driver type touches it, so a concurrently running test cannot consume the
    /// sender.
    static HUNG_HEALTH_CHECK_STARTED: std::sync::Mutex<Option<oneshot::Sender<()>>> =
        std::sync::Mutex::new(None);

    /// Never answers a health check and signals `HUNG_HEALTH_CHECK_STARTED` on entry.
    struct SignallingHungHealthCheckDriver;

    impl Driver for SignallingHungHealthCheckDriver {
        const NAME: &str = "signalling_hung_health_check_driver";

        async fn start(_context: DriverContext) -> Result<Self, DriverError> {
            Ok(Self)
        }
        async fn stop(&self) {}

        async fn health_check(&self, _request: HealthCheckRequest) -> HealthCheckResult {
            if let Some(tx) = HUNG_HEALTH_CHECK_STARTED.lock().unwrap().take() {
                let _ = tx.send(());
            }
            futures::future::pending().await
        }
    }

    #[test]
    fn test_stop_with_hung_health_check() {
        with_driver_server::<SignallingHungHealthCheckDriver, _, _>(
            "stop with hung health check",
            |client, _alerts| async move {
                client.start(DriverStartArgs::default()).await.unwrap().unwrap();

                let (started_tx, started_rx) = oneshot::channel();
                *HUNG_HEALTH_CHECK_STARTED.lock().unwrap() = Some(started_tx);
                let hc_client = client.clone();
                let hc_task = CurrentDispatcher
                    .compute(async move { hc_client.health_check(HealthCheckRequest::Fast).await });
                started_rx.await.unwrap();

                // Stop must complete even though the health check never does; the pending call
                // then fails because the server closes the channel once the driver has stopped.
                client.stop().await.unwrap();
                assert!(hc_task.await.unwrap().is_err());
            },
        );
    }

    /// Never answers a health check.
    struct HungHealthCheckDriver;

    impl Driver for HungHealthCheckDriver {
        const NAME: &str = "hung_health_check_driver";

        async fn start(_context: DriverContext) -> Result<Self, DriverError> {
            Ok(Self)
        }
        async fn stop(&self) {}

        async fn health_check(&self, _request: HealthCheckRequest) -> HealthCheckResult {
            futures::future::pending().await
        }
    }

    #[test]
    fn test_health_check_times_out() {
        with_driver_server::<HungHealthCheckDriver, _, _>(
            "health check times out",
            |client, _alerts| async move {
                client.start(DriverStartArgs::default()).await.unwrap().unwrap();

                // The driver never answers, so the server's own PING deadline (real time,
                // ~350ms) must produce the reply.
                let res = client.health_check(HealthCheckRequest::Ping).await.unwrap();
                assert_eq!(res, Err(HealthCheckError::Timeout));

                client.stop().await.unwrap();
            },
        );
    }

    /// Lets `test_forwards_health_alerts` report an alert from outside the driver after it has
    /// started, the way a driver's own background task would.
    static ALERTING_DRIVER_REPORTER: std::sync::Mutex<Option<HealthAlertReporter>> =
        std::sync::Mutex::new(None);

    /// Reports an alert from inside `start` and publishes its reporter for the test to report
    /// another later.
    struct AlertingDriver;

    impl Driver for AlertingDriver {
        const NAME: &str = "alerting_driver";

        async fn start(context: DriverContext) -> Result<Self, DriverError> {
            context.report_health_alert(HealthAlert::UpstreamNeighborFault(
                HealthCheckResponse::default(),
            ));
            *ALERTING_DRIVER_REPORTER.lock().unwrap() = Some(context.health_alert_reporter());
            Ok(Self)
        }
        async fn stop(&self) {}
    }

    #[test]
    fn test_forwards_health_alerts() {
        with_driver_server::<AlertingDriver, _, _>(
            "forwards health alerts",
            |client, mut alerts| async move {
                client.start(DriverStartArgs::default()).await.unwrap().unwrap();
                // The alert reported during `start` is sent over the `Driver` channel.
                assert!(matches!(alerts.next().await, Some(HealthAlert::UpstreamNeighborFault(_))));

                let reporter =
                    ALERTING_DRIVER_REPORTER.lock().unwrap().take().expect("driver started");
                reporter.report_health_alert(HealthAlert::SelfFault(HealthCheckResponse {
                    conditions: Some(vec![HealthCondition {
                        status: Some(HealthStatus::Unhealthy),
                        reason_code: Some("DMA_STALLED".to_string()),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }));
                let Some(HealthAlert::SelfFault(details)) = alerts.next().await else {
                    panic!("expected SelfFault");
                };
                assert_eq!(
                    details.conditions.unwrap()[0].reason_code.as_deref(),
                    Some("DMA_STALLED")
                );

                client.stop().await.unwrap();
            },
        );
    }
}
