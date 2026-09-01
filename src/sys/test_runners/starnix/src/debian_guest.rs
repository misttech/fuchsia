// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error, anyhow, bail};
use cm_types::NamespacePath;
use fidl::endpoints::{ClientEnd, Proxy};
use fidl_fuchsia_component_runner as frunner;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_virtualization::GuestConfig;
use fidl_fuchsia_virtualization_guest_interaction::{
    CommandListenerEvent, CommandListenerMarker, EnvironmentVariable, GuestType,
    InteractiveGuestMarker, InteractiveGuestProxy,
};
use fuchsia_async::{DurationExt, TimeoutExt};
use fuchsia_component::client::connect_to_protocol;
use fuchsia_fs::directory;
use futures::TryStreamExt;
use namespace::Namespace;
use std::cell::OnceCell;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const EXECUTE_TIMEOUT_SECONDS: i64 = 180;

// TODO(https://fxbug.dev/436831317): Execute within a proper working directory.
pub const GUEST_TEST_ROOT: &str = "/";
pub const GUEST_DATA_PATH: &str = "/data/tests/deps/";
pub const HOST_DATA_PATH: &str = "data/tests/deps/";

pub struct DebianGuest {
    instance_name: String,
    /// The proxy for interacting with the guest. This should be accessed by the `interactive_guest`
    /// helper function, to aid with locking and ensuring that the guest is ready for interaction.
    guest_proxy: OnceCell<Mutex<InteractiveGuestProxy>>,
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

    /// Gets a handle to the proxy, while also lazily bootstrapping the guest if necessary.
    async fn interactive_guest(&self) -> InteractiveGuestProxy {
        // Note that the OnceCell::get_or_init function doesn't play nicely with async init
        // functions, and I'm too lazy for an OSRB review for the async_once_cell. So we'll do
        // some manually juggling here to initialize the "ole fashioned way.""
        match self.guest_proxy.get() {
            Some(proxy_mutex) => proxy_mutex.lock().unwrap().clone(),
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

                let guest_proxy = connect_to_protocol::<InteractiveGuestMarker>()
                    .expect("Error connecting to InteractiveGuest");
                guest_proxy
                    .start(GuestType::Debian, &self.instance_name, cfg)
                    .await
                    .expect("Debian guest failed to start!");

                let return_proxy = guest_proxy.clone();

                let proxy_mutex = Mutex::new(guest_proxy);
                self.guest_proxy
                    .set(proxy_mutex)
                    .expect("Unexpected race condition while bootstrapping the guest proxy.");

                return_proxy
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
        destination: &Path,
    ) -> Result<(), Error> {
        log::info!(tag = self.instance_name.as_str(); "Pushing data to guest (destination: {})", destination.display());

        let dest_str = destination
            .to_str()
            .ok_or_else(|| anyhow!("Destination path is not valid UTF-8: {:?}", destination))?;
        let guest_proxy = self.interactive_guest().await;
        let response = guest_proxy
            .put_file(source, dest_str)
            .await
            .context("FIDL call to InteractiveGuest::PutFile has failed.")?;

        if let Err(status) = zx::Status::ok(response) {
            bail!("PutFile operation failed with status: {:?}", status);
        }

        log::info!(tag = self.instance_name.as_str();
            "Successfully pushed data to guest (destination: {})",
            destination.display()
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
        remote_path: &Path,
        local_file_proxy: ClientEnd<fio::FileMarker>,
    ) -> Result<(), Error> {
        log::info!(tag = self.instance_name.as_str(); "Fetching file from guest (remote_path: {})", remote_path.display());
        let remote_path_str = remote_path
            .to_str()
            .ok_or_else(|| anyhow!("Remote path is not valid UTF-8: {:?}", remote_path))?;
        let guest_proxy = self.interactive_guest().await;

        let response = guest_proxy
            .get_file(remote_path_str, local_file_proxy)
            .await
            .context("FIDL call to GetFile failed")?;

        if let Err(status) = zx::Status::ok(response) {
            bail!("GetFile operation failed with status: {:?}", status);
        }
        Ok(())
    }

    /// Executes a command on the guest, returning the command's exit code upon successful execution
    /// and an Error if the command was unable to be executed on the guest.
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
    ) -> Result<i32, Error> {
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
                    CommandListenerEvent::OnStarted { status } => match zx::Status::ok(status) {
                        Ok(()) => {
                            log::info!(tag = self.instance_name.as_str(); "Command '{}'\n...started successfully", command)
                        }
                        Err(status) => {
                            bail!("Command '{}'\n...failed to start: {:?}", command, status)
                        }
                    },
                    CommandListenerEvent::OnTerminated { status, return_code } => {
                        let term_status = zx::Status::err_from_raw(status);
                        log::info!(tag = self.instance_name.as_str();
                            "Command '{}'\n...terminated with status {:?}, return code {}",
                            command,
                            term_status,
                            return_code
                        );
                        if let Err(status) = zx::Status::ok(status) {
                            bail!("Command '{}' failed with status: {:?}", command, status);
                        }
                        return Ok(return_code);
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
                let proxy_clone = proxy.lock().unwrap().clone();
                proxy_clone.shutdown().await.context("FIDL call to Shutdown failed")
            }
            None => {
                log::info!(tag = self.instance_name.as_str(); "Guest was never bootstrapped, shutdown is unnecessary.");
                Ok(())
            }
        }
    }

    pub fn are_deps_pushed(&self) -> bool {
        self.deps_pushed.get().is_some()
    }

    /// Expected to be called once and only once.
    pub fn mark_deps_pushed(&self) {
        self.deps_pushed.set(true).expect("Unexpected state management, test dependencies are expected to be pushed once, and only once.");
    }

    /// Gets the absolute guest filepath for test results given a unique filename.
    pub fn get_test_output_path(guest_output_filename: &str) -> PathBuf {
        Path::new(GUEST_TEST_ROOT).join(guest_output_filename)
    }

    /// Gets the absolute guest filepath for the test binary given the host source location.
    pub fn get_test_binary_path(source_location: &str) -> Result<PathBuf, Error> {
        let binary_name = Path::new(source_location)
            .file_name()
            .ok_or_else(|| anyhow!("Binary path format was unexpected."))?;
        Ok(Path::new(GUEST_TEST_ROOT).join(binary_name))
    }

    /// Pushes the test binary and all data dependencies to the guest if not already pushed.
    pub async fn push_test_dependencies(
        &self,
        mut test_component_ns: Namespace,
        test_start_info: &frunner::ComponentStartInfo,
    ) -> Result<PathBuf, Error> {
        let test_pkg_dir = test_component_ns
            .remove(&NamespacePath::new("/pkg")?)
            .ok_or_else(|| anyhow!("Could not find /pkg in namespace!"))?
            .into_proxy();

        if !self.are_deps_pushed() {
            self.push_data_deps(&test_pkg_dir).await?;
        }

        self.push_test_binary(test_start_info, &test_pkg_dir).await
    }

    /// Pushes data dependencies from `/pkg/data/tests/deps/` to the guest's `/data/tests/deps/`.
    pub async fn push_data_deps(&self, pkg_dir_proxy: &fio::DirectoryProxy) -> Result<(), Error> {
        let deps_dir = match directory::open_directory(
            pkg_dir_proxy,
            HOST_DATA_PATH,
            fio::PERM_READABLE,
        )
        .await
        {
            Ok(dir) => dir,
            Err(e) => {
                log::info!("No test deps directory found at {}: {:?}", HOST_DATA_PATH, e);
                self.mark_deps_pushed();
                return Ok(());
            }
        };

        let entries = directory::readdir(&deps_dir).await?;
        for entry in entries {
            match entry.kind {
                directory::DirentKind::File => {
                    let file_name = entry.name;
                    let source_file =
                        directory::open_file(&deps_dir, &file_name, fio::PERM_READABLE)
                            .await
                            .with_context(|| format!("Failed to open dep file: {}", file_name))?;

                    let source = source_file.into_client_end().map_err(|s| {
                        anyhow!("Failed to convert source file to client end: {:?}", s)
                    })?;

                    let guest_dest = format!("{}{}", GUEST_DATA_PATH, file_name);
                    let guest_dest_path = Path::new(&guest_dest);
                    self.push_data_to_guest(source, &guest_dest_path).await?;
                }
                _ => {
                    bail!("Unexpected file/folder structure in deps folder: {}", entry.name);
                }
            }
        }

        self.mark_deps_pushed();
        Ok(())
    }

    /// Pushes the test binary from ComponentStartInfo to the Debian guest.
    pub async fn push_test_binary(
        &self,
        test_start_info: &frunner::ComponentStartInfo,
        pkg_dir_proxy: &fio::DirectoryProxy,
    ) -> Result<PathBuf, Error> {
        let host_binary_location = runner::get_program_binary(test_start_info)?;
        let guest_dest = Self::get_test_binary_path(&host_binary_location)?;

        let source =
            directory::open_file(pkg_dir_proxy, host_binary_location.as_str(), fio::PERM_READABLE)
                .await?
                .into_client_end()
                .map_err(|_| anyhow!("Converting test bin file to client end failed"))?;

        self.push_data_to_guest(source, &guest_dest).await?;
        Ok(guest_dest)
    }
}
