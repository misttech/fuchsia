// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::result::Result::Err;

use anyhow::{Context, Error, anyhow, bail};
use fidl::endpoints::ClientEnd;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_virtualization::GuestConfig;
use fidl_fuchsia_virtualization_guest_interaction::{
    CommandListenerEvent, CommandListenerMarker, EnvironmentVariable, InteractiveDebianGuestMarker,
    InteractiveDebianGuestProxy,
};
use fuchsia_async::{DurationExt, TimeoutExt};
use fuchsia_component::client::connect_to_protocol;
use futures::TryStreamExt;
use std::cell::OnceCell;
use std::sync::{Mutex, MutexGuard};

const EXECUTE_TIMEOUT_SECONDS: i64 = 180;
pub struct DebianGuest {
    instance_name: String,
    /// The proxy for interacting with the guest. This should be accessed by the `interactive_guest`
    /// helper function, to aid with locking and ensuring that the guest is ready for interaction.
    guest_proxy: OnceCell<Mutex<InteractiveDebianGuestProxy>>,
    /// Stores the state of whether test data dependencies have already been pushed to the guest.
    // TODO(https://fxbug.dev/438284662): Better state / lifecycle management.
    deps_pushed: OnceCell<bool>,
}

impl DebianGuest {
    /// Creates a new instance of the DebianGuest. The actual bootstrapping of a guest is done
    /// lazily. This is because the lifecycle of the DebianGuest needs to live through the Starnix
    /// test runner framework, but not all tests will actually need the guest. Thus, we construct
    /// the DebianGuest while refraining from bootstrap until the guest is first interacted with.
    ///
    /// # Arguments
    /// * `instance_name` - An instance name, which serves as the tag for log output.
    pub fn new(instance_name: String) -> DebianGuest {
        DebianGuest { instance_name, guest_proxy: OnceCell::new(), deps_pushed: OnceCell::new() }
    }

    /// Gets a mutex handle on the proxy, while also lazily bootstrapping the guest if necessary.
    async fn interactive_guest(&self) -> MutexGuard<'_, InteractiveDebianGuestProxy> {
        // Note that the OnceCell::get_or_init function doesn't play nicely with async init
        // functions, and I'm too lazy for an OSRB review for the async_once_cell. So we'll do
        // some manually juggling here to initialize the "ole fashioned way.""
        match self.guest_proxy.get() {
            Some(proxy_mutex) => proxy_mutex.lock().unwrap(),
            None => {
                log::info!(tag = self.instance_name.as_str();
                    "Interaction requested, lazily starting the guest instance."
                );

                let mut cfg = GuestConfig::default();
                cfg.virtio_gpu = Some(false);
                cfg.virtio_sound = Some(false);
                cfg.virtio_sound_input = Some(false);
                cfg.virtio_rng = Some(false);
                cfg.virtio_balloon = Some(false);
                cfg.virtio_mem = Some(false);
                cfg.default_net = Some(false);

                let guest_proxy = connect_to_protocol::<InteractiveDebianGuestMarker>()
                    .expect("Error connecting to InteractiveDebianGuest");
                guest_proxy
                    .start(&self.instance_name, cfg)
                    .await
                    .expect("Debian guest failed to start!");

                let proxy_mutex = Mutex::new(guest_proxy);
                self.guest_proxy
                    .set(proxy_mutex)
                    .expect("Unexpected race condition while bootstrapping the guest proxy.");
                self.guest_proxy.get().unwrap().lock().unwrap()
            }
        }
    }

    /// Pushes data from `source` to the guest at `destination`.
    ///
    /// # Arguments
    /// * `source` - The source file to copy from.
    /// * `destination` - The destination path in the guest's filesystem.
    pub async fn push_data_to_guest(
        &self,
        source: ClientEnd<fidl_fuchsia_io::FileMarker>,
        destination: &String,
    ) -> Result<(), Error> {
        log::info!(tag = self.instance_name.as_str(); "Pushing data to guest (destination: {})", destination);

        let response = self
            .interactive_guest()
            .await
            .put_file(source, destination.as_str())
            .await
            .context("FIDL call to InteractiveDebianGuest::PutFile has failed.")?;

        let push_result = zx::Status::from_raw(response);
        if push_result != zx::Status::OK {
            bail!("PutFile operation failed with status: {:?}", push_result);
        }

        log::info!(tag = self.instance_name.as_str();
            "Successfully pushed data to guest (destination: {})",
            destination
        );

        Ok(())
    }

    /// Fetches a file from the guest.
    ///
    /// # Arguments
    /// * `remote_path` - The path to the file in the guest's filesystem.
    /// * `local_file_proxy` - The local file proxy to write the contents to.
    pub async fn get_file(
        &self,
        remote_path: &str,
        local_file_proxy: ClientEnd<fio::FileMarker>,
    ) -> Result<(), Error> {
        log::info!(tag = self.instance_name.as_str(); "Fetching file from guest (remote_path: {})", remote_path);
        let response = self
            .interactive_guest()
            .await
            .get_file(remote_path, local_file_proxy)
            .await
            .context("FIDL call to GetFile failed")?;

        let result_status = zx::Status::from_raw(response);
        if result_status != zx::Status::OK {
            bail!("GetFile operation failed with status: {:?}", result_status);
        }
        Ok(())
    }

    /// Executes a command on the guest.
    ///
    /// # Arguments
    /// * `command`: The command string to execute (e.g., "/bin/ls -l /tmp").
    /// * `env_vars`: Environment vars to set for the execution context.
    /// * `stdin`: An optional `zx::Socket` for providing standard input to the command.
    /// * `stdout`: An optional client end for receiving stdout from the command.
    /// * `stderr`: An optional client end for receiving stderr from the command.
    pub async fn execute(
        &self,
        command: &str,
        env_vars: &[EnvironmentVariable],
        stdin: Option<zx::Socket>,
        stdout: Option<zx::Socket>,
        stderr: Option<zx::Socket>,
    ) -> Result<(), Error> {
        log::info!(tag = self.instance_name.as_str(); "Executing command on guest: {})", command);

        let (command_listener_client, command_listener_server) =
            fidl::endpoints::create_proxy::<CommandListenerMarker>();

        self.interactive_guest()
            .await
            .execute_command(command, env_vars, stdin, stdout, stderr, command_listener_server)
            .context("FIDL call to ExecuteCommand failed")?;

        let mut event_stream = command_listener_client.take_event_stream();

        let execution_future = async move {
            while let Some(event) = event_stream.try_next().await? {
                match event {
                    CommandListenerEvent::OnStarted { status } => {
                        let start_status = zx::Status::from_raw(status);
                        match start_status {
                            zx::Status::OK => {
                                log::info!(tag = self.instance_name.as_str(); "Command '{}'\n...started with status: {:?}", command, start_status)
                            }
                            _ => bail!(
                                "Command '{}'\n...failed to start: {:?}",
                                command,
                                start_status
                            ),
                        }
                    }
                    CommandListenerEvent::OnTerminated { status, return_code } => {
                        let term_status = zx::Status::from_raw(status);
                        log::info!(tag = self.instance_name.as_str();
                            "Command '{}'\n...terminated with status {:?}, return code {}",
                            command,
                            term_status,
                            return_code
                        );
                        return Ok(());
                    }
                }
            }

            panic!("Execution result stream closed before OnTerminated event!");
        };

        let timeout_duration = zx::MonotonicDuration::from_seconds(EXECUTE_TIMEOUT_SECONDS);
        execution_future
            .on_timeout(timeout_duration.after_now(), || {
                Err(anyhow!(
                    "Command execution '{}'\n...timed out after {} seconds",
                    command,
                    EXECUTE_TIMEOUT_SECONDS
                ))
            })
            .await
    }

    /// Shuts down the guest.
    pub async fn shutdown(&self) -> Result<(), Error> {
        match self.guest_proxy.get() {
            Some(proxy) => {
                log::info!(tag = self.instance_name.as_str(); "Shutting down guest instance.");
                proxy.lock().unwrap().shutdown().await.context("FIDL call to Shutdown failed")
            }
            None => {
                log::info!(tag = self.instance_name.as_str(); "Guest was never bootstrapped, shutdown is unnecessary.");
                Ok(())
            }
        }
    }

    pub fn are_deps_pushed(&self) -> bool {
        return self.deps_pushed.get() != None;
    }

    /// Expected to be called once and only once.
    pub fn mark_deps_pushed(&self) {
        self.deps_pushed.set(true).expect("Unexpected state management, test dependencies are expected to be pushed once, and only once.");
    }
}
