// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::ffi::c_void;
use core::ptr::NonNull;
use std::num::NonZero;
use std::sync::OnceLock;

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

use crate::{Driver, DriverContext, DriverError};
use fdf_sys::fdf_dispatcher_get_current_dispatcher;
use fuchsia_async::LocalExecutorBuilder;

/// The driver transport the lifecycle server reads on. Reads are registered against the root
/// dispatcher's always-on interface so that requests (notably `Resume`) keep being delivered
/// while the driver's dispatchers are suspended.
type Transport = DriverChannel<AsyncDispatcher>;

/// Implements the lifecycle management of a rust driver, including starting and stopping it
/// and setting up the rust async dispatcher and logging for the driver to use, and running a
/// message loop for the driver start and stop messages.
pub struct DriverServer<T> {
    /// Holds the lifecycle channel until [`Self::message_loop`] hands it to the fidl_next server.
    server_handle: OnceLock<Channel<[fidl_next::Chunk]>>,
    root_dispatcher: DriverDispatcherRef<'static>,
    driver: OnceLock<T>,
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
        let handler = LifecycleHandler { server: Some(self), fidl_server: server.server() };
        match server.run_local(handler).await {
            // The driver host closed the channel without sending `Stop`.
            Ok(mut handler) if handler.server.is_some() => {
                warn!(
                    "Driver server channel closed before a stop message; \
                     stop() will not be called."
                );
                handler.finish();
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
    async fn handle_start(&self, start_args: DriverStartArgs) -> Result<(), Status> {
        let context = DriverContext::new(AsyncDispatcher::new(&self.root_dispatcher), start_args)?;
        context.start_logging(T::NAME)?;

        log::debug!("driver starting");

        let driver = T::start(context).await.map_err(DriverError::log_to_status)?;
        self.driver.set(driver).map_err(|_| ()).expect("Driver received start message twice");
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
        self.driver.get()
    }
}

/// The `fidl_next` server handler for the driver lifecycle protocol. It borrows the
/// [`DriverServer`] for the duration of [`DriverServer::message_loop`] only.
struct LifecycleHandler<'a, T> {
    /// `None` once the driver has stopped or failed to start, after which the server must not be
    /// touched: the driver host is free to destroy it as soon as the channel closes.
    server: Option<&'a mut DriverServer<T>>,
    fidl_server: Server<DriverProtocol, Transport>,
}

impl<T: Driver> LifecycleHandler<'_, T> {
    /// Tears down the driver and closes the channel. This is the last thing the handler does
    /// with the server: the driver host treats the channel closing as the signal that the driver
    /// is fully stopped.
    fn finish(&mut self) {
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
        let res = server.handle_start(request.payload().start_args).await;
        let reply = match res {
            Ok(()) => responder.respond(()).await,
            Err(status) => responder.respond_err(status).await,
        };
        if let Err(e) = reply {
            error!("Failed to reply to Start: {e}, exiting main loop.");
            self.finish();
        } else if res.is_err() {
            debug!("driver failed to start, exiting main loop");
            self.finish();
        }
    }

    async fn stop(&mut self) {
        if let Some(server) = self.server.as_deref_mut() {
            server.handle_stop().await;
        }
        self.finish();
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
        _request: Request<driver::HealthCheck, Transport>,
        responder: Responder<driver::HealthCheck, Transport>,
    ) {
        // Health checks are not yet supported by the Rust driver SDK.
        log_reply("HealthCheck", responder.respond_err(HealthCheckError::NotSupported).await);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DriverError;

    use fdf::{CurrentDispatcher, OnDispatcher};
    use fdf_env::test::spawn_in_driver;
    use fidl_next_fuchsia_driver_framework::DriverClientHandler;
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

    fn with_driver_server<F, Fut>(name: &str, body: F)
    where
        F: FnOnce(TestClient) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let initialize_func = __fuchsia_driver_registration__.v1.initialize.expect("initializer");
        let destroy_func = __fuchsia_driver_registration__.v1.destroy.expect("destroy function");
        let (server_chan, client_chan) = Channel::<[fidl_next::Chunk]>::create();

        spawn_in_driver(name, async move {
            let client_end: ClientEnd<DriverProtocol, _> =
                ClientEnd::from_untyped(DriverChannel::new(client_chan));
            let dispatcher = ClientDispatcher::new(client_end);
            let client = dispatcher.client();
            // Dropping the last `Client` stops the client dispatcher, so keep one alive until the
            // server has closed the channel regardless of what `body` does with its copy.
            let keepalive = client.clone();

            let client_task = CurrentDispatcher.spawn(async move {
                // The dispatcher exits with `PeerClosed` once the server releases its end of the
                // channel, which is when the driver has finished stopping (or failed to start).
                dispatcher.run(DriverClient).await.unwrap_err();
            });

            let channel_handle = server_chan.into_driver_handle().into_raw().get();
            // SAFETY: `channel_handle` is a valid, non-zero FDF channel handle created above.
            // Ownership of the handle is given to `initialize_func`.
            let driver_server = unsafe { initialize_func(channel_handle) } as usize;
            assert_ne!(driver_server, 0);

            body(client).await;

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
        with_driver_server("stop before start", |client| async move {
            client.stop().await.unwrap();
        });
    }

    #[test]
    fn test_requests_queued_behind_start() {
        with_driver_server("requests queued behind start", |client| async move {
            let start = client.start(DriverStartArgs::default());
            let suspend = client.suspend();
            let resume = client.resume(None::<zx::EventPair>);
            let stop = client.stop();
            let (start, suspend, resume, stop) = futures::join!(start, suspend, resume, stop);
            start.unwrap().unwrap();
            suspend.unwrap().unwrap();
            resume.unwrap().unwrap();
            stop.unwrap();
        });
    }
}
