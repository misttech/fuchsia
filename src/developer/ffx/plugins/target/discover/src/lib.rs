// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use ffx_config::EnvironmentContext;
use ffx_target_discover_args::DiscoverCommand;
use ffx_writer::SimpleWriter;
use fho::{FfxMain, FfxTool, Result, return_user_error, user_error};
use fuchsia_async::Timer;
use futures::channel::mpsc;
use futures::executor::block_on;
use futures::{FutureExt, SinkExt, Stream, StreamExt};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;
use tokio::signal::unix::{Signal, SignalKind, signal};

const PID_FILE: &str = "discover.pid";

#[cfg_attr(test, mockall::automock)]
trait ProcessManager {
    fn is_running(&self, pid: u32) -> bool;
    fn kill_pid(&self, pid: u32, signal: nix::sys::signal::Signal) -> Result<(), nix::Error>;
    fn daemonize(&self) -> Result<()>;
    fn get_pid(&self) -> u32;
}

struct SystemProcessManager;

impl ProcessManager for SystemProcessManager {
    fn is_running(&self, pid: u32) -> bool {
        let nix_pid = nix::unistd::Pid::from_raw(pid as i32);
        // First do a no-hand wait to collect the process if it's defunct.
        let _ = nix::sys::wait::waitpid(nix_pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG));
        nix::sys::signal::kill(nix_pid, None).is_ok()
    }

    fn kill_pid(&self, pid: u32, signal: nix::sys::signal::Signal) -> Result<(), nix::Error> {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), signal)
    }

    fn daemonize(&self) -> Result<()> {
        // Copied from tools/usb_driver/sr/lib.rs. Librarify?
        // daemonize(3) is deprecated on macOS 10.15. The replacement is not
        // yet clear, we may want to replace this with a manual double fork
        // setsid, etc.
        #[allow(deprecated)]
        // First argument: chdir(/)
        // Second argument: close stdio
        //
        // SAFETY: This shouldn't do much of anything to memory state. If it
        // succeeds we've effectively just been shuffled around the process
        // table. If it fails then it likely has no side effects at all.
        match unsafe { libc::daemon(0, 0) } {
            0 => Ok(()),
            x => Err(anyhow!(std::io::Error::from_raw_os_error(x)).into()),
        }
    }

    fn get_pid(&self) -> u32 {
        std::process::id()
    }
}

#[async_trait(?Send)]
trait DiscoveryRunner {
    async fn run_discovery(&self) -> Result<()>;
}

#[derive(Clone, Copy, Debug)]
enum Output {
    All,
    Error,
    None,
}
struct RealDiscoveryRunner {
    context: EnvironmentContext,
    output_mode: Output,
}

#[async_trait(?Send)]
impl DiscoveryRunner for RealDiscoveryRunner {
    async fn run_discovery(&self) -> Result<()> {
        if matches!(self.output_mode, Output::All) {
            println!("Discovered devices:");
        }
        let discovery =
            ffx_target::build_discovery(discovery::DiscoverySources::all(), true, &self.context);
        let devices = discovery.create_cache().await.context("Could not create cache")?;
        if matches!(self.output_mode, Output::All) {
            for h in devices {
                print_device(h);
            }
        }
        Ok(())
    }
}

#[derive(FfxTool)]
#[target(None)]
pub struct DiscoverTool {
    #[command]
    cmd: DiscoverCommand,
    context: EnvironmentContext,
}

fho::embedded_plugin!(DiscoverTool);

// Make generic for testing purposes
struct Discoverer<P: ProcessManager, D: DiscoveryRunner> {
    context: EnvironmentContext,
    process_manager: P,
    discovery_runner: D,
    cache_dir: PathBuf,
    pid_path: PathBuf,
    foreground: bool,
    output_mode: Output,
}

#[async_trait(?Send)]
impl FfxMain for DiscoverTool {
    type Writer = SimpleWriter;
    async fn main(self, _writer: Self::Writer) -> Result<()> {
        // Run quietly if we're in the background, or if the user requested it
        let mut discoverer = Discoverer::new(
            self.context,
            self.cmd.foreground,
            self.cmd.quiet,
            SystemProcessManager,
        )?;
        discoverer.discover(self.cmd).await
    }
}

impl<P: ProcessManager> Discoverer<P, RealDiscoveryRunner> {
    fn new(
        context: EnvironmentContext,
        foreground: bool,
        quiet: bool,
        process_manager: P,
    ) -> Result<Self> {
        let Some(cache_dir) = ffx_target::get_discovery_cache_dir(&context) else {
            return_user_error!("Error: No cache dir set");
        };
        fs::create_dir_all(&cache_dir)
            .context(format!("Creating cache_dir {}", cache_dir.display()))?;
        let mut pid_path = cache_dir.clone();
        pid_path.push(PID_FILE);
        // Even in "quiet" mode, we still want to see errors when in foreground mode
        let output_mode = if quiet { Output::Error } else { Output::All };
        let discovery_runner = RealDiscoveryRunner { context: context.clone(), output_mode };
        Ok(Self {
            context,
            cache_dir,
            pid_path,
            foreground,
            output_mode,
            process_manager,
            discovery_runner,
        })
    }
}

impl<P: ProcessManager, D: DiscoveryRunner> Discoverer<P, D> {
    #[cfg(test)]
    fn new_with_runner(
        context: EnvironmentContext,
        foreground: bool,
        process_manager: P,
        discovery_runner: D,
    ) -> Result<Self> {
        let Some(cache_dir) = ffx_target::get_discovery_cache_dir(&context) else {
            return_user_error!("Error: No cache dir set");
        };
        // Only produce output when running in the foreground
        fs::create_dir_all(&cache_dir)
            .with_context(|| format!("Creating cache_dir {}", cache_dir.display()))?;
        let mut pid_path = cache_dir.clone();
        pid_path.push(PID_FILE);
        Ok(Self {
            context: context.clone(),
            cache_dir,
            pid_path,
            foreground,
            output_mode: Output::None,
            process_manager,
            discovery_runner,
        })
    }

    async fn discover(&mut self, cmd: DiscoverCommand) -> Result<()> {
        // If the "stop" flag is passed, that takes precedence. Just try to stop
        // the background process.
        if cmd.stop {
            self.stop_process()?;
            return self.remove_cache_file();
        }

        let duration = match cmd.time {
            None => discovery::get_cache_recheck_time(),
            Some(t) => Duration::from_secs(t),
        };
        if duration == Duration::ZERO {
            if !cmd.foreground {
                return_user_error!(
                    "Error: Non-zero interval must be specified when running in background"
                );
            }
            return self.discovery_runner.run_discovery().await;
        }
        if self.do_process_management().await? {
            // Returns true if we should exit
            return Ok(());
        };

        let mut signal_stream = SignalStream::new(
            signal(SignalKind::user_defined1()).context("Couldn't create SIGUSR1 listener")?,
        );
        self.run_loop(duration, &mut signal_stream).await
    }

    // Loop, doing discovery. Stop if our pid file disappears. Rediscover after
    // our timer goes off, or after we get signalled.
    async fn run_loop<S>(&self, duration: Duration, signal_stream: &mut S) -> Result<()>
    where
        S: Stream<Item = ()> + Unpin,
    {
        let (quit_tx, mut quit_rx) = mpsc::channel(1);
        let _pid_watcher =
            self.start_pid_watcher(quit_tx).await.context("Starting pid file watcher")?;
        loop {
            futures::select! {
                _ = Timer::new(duration).fuse() => {},
                _ = signal_stream.next().fuse() => {},
                _ = quit_rx.next().fuse() => {
                    self.out("pid file deleted");
                    break;
                 },
            }
            self.discovery_runner.run_discovery().await?;
        }
        if Path::exists(&self.pid_path) {
            if let Err(e) = self.remove_pid_file() {
                self.err(&format!("failed to remove pid file: {e}"));
            }
        }
        Ok(())
    }

    // Stop the background process, by removing the pid file. We'll do a few
    // checks along the way:
    // * If there's no pid file, we assume there's no process
    // * If the pid file is corrupt, remove it and warn the user
    // * If the process isn't running, inform the user and remove the file anyway
    // * If it is running, notify the user that we are stopping the process
    fn stop_process(&self) -> Result<()> {
        if !Path::exists(&self.pid_path) {
            self.out("No pid file for discovery process");
            return Ok(());
        }
        let Some(pid) = self.maybe_get_pid() else {
            return Ok(());
        };
        if !self.process_manager.is_running(pid) {
            self.out(&format!(
                "Process {pid} wasn't running; removing {} before restarting.",
                self.pid_path.display()
            ));
        } else {
            self.out(&format!("Stopping {pid}"));
        }
        self.remove_pid_file()
    }

    fn maybe_get_pid(&self) -> Option<u32> {
        match self.get_pid_from_file() {
            Ok(pid) => Some(pid),
            Err(e) => {
                self.err(&format!("Got error reading pid path: {e:?}."));
                self.err(&format!("Removing {}", self.pid_path.display()));
                self.err("You will have to stop any discovery process by hand.");
                if let Err(e) = self.remove_pid_file() {
                    self.err(&format!("Failed to remove {}: {e:?}", self.pid_path.display()));
                }
                None
            }
        }
    }

    fn remove_pid_file(&self) -> Result<()> {
        fs::remove_file(&self.pid_path)
            .map_err(|e| fho::Error::from(anyhow!("failed to remove pid file: {e}")))
    }

    async fn start_pid_watcher(&self, quit_tx: mpsc::Sender<()>) -> Result<RecommendedWatcher> {
        let file_path = self.pid_path.to_owned();
        let watcher = self.start_file_watcher(
            |kind| matches!(kind, notify::event::EventKind::Remove(_)),
            file_path,
            quit_tx,
        )?;
        Ok(watcher)
    }

    // Adapted from daemon/server/src/daemon.rs
    fn start_file_watcher(
        &self,
        kind_matcher: impl Fn(notify::event::EventKind) -> bool + Send + 'static,
        file_path: PathBuf,
        mut tx: mpsc::Sender<()>,
    ) -> Result<RecommendedWatcher> {
        let event_handler = move |res| {
            block_on(async {
                use notify::event::Event;
                match res {
                    Ok(Event { kind, paths, .. })
                        if kind_matcher(kind) && paths.contains(&file_path) =>
                    {
                        tx.send(()).await.ok();
                    }
                    Err(ref e @ notify::Error { ref kind, .. }) => {
                        match kind {
                            notify::ErrorKind::Io(ioe) => {
                                log::warn!("IO error. Ignoring {ioe:?}");
                            }
                            _ => {
                                // If we get a non-spurious error, treat that as something that
                                // should cause us to exit.
                                log::warn!("exiting due to file watcher error: {e:?}");
                                tx.send(()).await.ok();
                            }
                        }
                    }
                    Ok(_) => {} // just ignore any non-delete event or for any other file.
                }
            })
        };
        #[cfg(target_os = "macos")]
        let res = RecommendedWatcher::new(
            event_handler,
            notify::Config::default().with_poll_interval(Duration::from_millis(500)),
        );
        #[cfg(not(target_os = "macos"))]
        let res = RecommendedWatcher::new(event_handler, notify::Config::default());
        let mut watcher = res.context("Creating watcher")?;
        watcher
            .watch(&self.cache_dir, RecursiveMode::NonRecursive)
            .context("Setting watcher context")?;
        Ok(watcher)
    }

    // This function aims to encapsulate the following logic:
    // * if there is a pid file, read it, and:
    //   * if it's corrupt, report and exit, since this is potentially a bigger issue
    //   * if there's the pid isn't running, report that, remove the pid file, and keep doing discovery
    //   * if user wants us to run in the foreground, report an error since there is already a background
    //     process
    //   * if everything is hunky-dory, send a signal to the process and exit
    // * we are doing discovery ourselves. If in the foreground:
    //   * just write our own pid into the pid file, and return
    // * otherwise, fork into a daemon, and write the child's pid into the pid file
    // When we return, we'll either exit, or continue to do discovery
    async fn do_process_management(&mut self) -> Result<bool> {
        if Path::exists(&self.pid_path) {
            let Some(pid) = self.maybe_get_pid() else {
                return Ok(true);
            };
            if !self.process_manager.is_running(pid) {
                self.out(&format!(
                    "Process {pid} wasn't running; removing {}",
                    self.pid_path.display()
                ));
                self.remove_pid_file()?;
            // Otherwise, continue and start a new discovery process
            } else {
                if self.foreground {
                    return_user_error!(
                        "Error: Background discovery is already running (in process {pid})"
                    );
                }
                // Send SIGUSR1 message, to ask the process to reinitiate discovery
                if let Err(e) =
                    self.process_manager.kill_pid(pid, nix::sys::signal::Signal::SIGUSR1)
                {
                    self.err(&format!("Couldn't signal {pid}: {e}"));
                } else {
                    self.out(&format!(
                        "Background discovery process {pid} is already running. Requesting that it re-populate the cache..."
                    ));
                    self.wait_for_new_cache().await?;
                    self.out("Done.");
                }
                return Ok(true);
            }
        }

        // Run once even before we fork, so user is sure that the cache is available for the
        // next command.
        self.discovery_runner.run_discovery().await?;

        if self.foreground {
            self.write_pid()?;
            return Ok(false);
        }

        self.out(
            "Running discovery as background process. Use \"ffx target discover --stop\" to stop",
        );
        // Now that we'll be in the background, make sure everything else in the
        // background process is quiet
        self.output_mode = Output::None;

        self.process_manager.daemonize()?;
        self.write_pid()?;
        Ok(false)
    }

    fn cache_file_path(&self) -> PathBuf {
        let mut cache_path = self.cache_dir.clone();
        cache_path.push(discovery::CACHE_FILE_NAME);
        cache_path
    }

    // Wait for the cache to appear, before returning to the user
    async fn wait_for_new_cache(&self) -> Result<()> {
        let cache_path = self.cache_file_path();
        let (tx, mut rx) = mpsc::channel(1);
        use notify::event;
        let _watcher = self.start_file_watcher(
            // Let's use "Any" so we're not tied to the caching library modifying vs. re-creating the cache
            |kind| matches!(kind, event::EventKind::Modify(_) | event::EventKind::Create(_)),
            cache_path,
            tx,
        )?;
        rx.next().await;
        Ok(())
    }

    fn get_pid_from_file(&self) -> Result<u32> {
        let pid_str = fs::read_to_string(&self.pid_path).context("reading pid file")?;
        let pid = pid_str.trim().parse::<u32>().context("parsing pid file")?;
        Ok(pid)
    }

    fn write_pid(&self) -> Result<()> {
        let pid = self.process_manager.get_pid();
        let mut file = fs::File::create(&self.pid_path).context("creating pid file")?;
        let line = format!("{pid}");
        file.write_all(line.as_bytes()).context("writing to pid file")?;
        Ok(())
    }

    fn out(&self, arg: &str) {
        if matches!(self.output_mode, Output::All) {
            println!("{arg}");
        }
    }

    fn err(&self, arg: &str) {
        if !matches!(self.output_mode, Output::None) {
            eprintln!("{arg}");
        }
    }

    fn remove_cache_file(&self) -> Result<()> {
        let discovery = discovery::DiscoveryBuilder::default()
            .with_cache_dir(Some(self.cache_dir.clone()))
            .build(&self.context);
        discovery.delete_cache().map_err(|e| user_error!("Could not remove cache: {e}"))
    }
}

// Functions for formatting the discovery results
fn format_addrs<T: std::fmt::Display>(addrs: Vec<T>) -> String {
    addrs.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(",")
}
fn format_state(state: discovery::TargetState) -> String {
    match state {
        discovery::TargetState::Unknown => "unknown".to_string(),
        discovery::TargetState::Product { addrs, serial } => {
            let addrs_s = format_addrs(addrs);
            let serial_s = match serial {
                Some(s) => format!(", serial:{s}"),
                None => "".to_string(),
            };
            format!("{addrs_s}{serial_s} (product)")
        }
        discovery::TargetState::Fastboot(fastboot_target_state) => {
            let conn_s = match fastboot_target_state.connection_state {
                discovery::FastbootConnectionState::Usb => "usb".to_string(),
                discovery::FastbootConnectionState::Tcp(addrs) => {
                    let addrs_s = format_addrs(addrs);
                    format!("tcp:{addrs_s}")
                }
                discovery::FastbootConnectionState::Udp(addrs) => {
                    let addrs_s = format_addrs(addrs);
                    format!("udp:{addrs_s}")
                }
            };
            format!("serial:{}, {conn_s} (fastboot)", fastboot_target_state.serial_number)
        }
        discovery::TargetState::Zedboot => "zedboot".to_string(),
    }
}
fn print_device(h: discovery::TargetHandle) {
    let node_s = match h.node_name {
        Some(name) => name,
        None => "<unknown>".to_string(),
    };
    println!("{node_s}: {}", format_state(h.state));
}

// Minimal version of tokio-stream::wrappers::SignalStream, since we don't currently have that
// crate compiled with the "signal" feature.
struct SignalStream(Signal);
impl SignalStream {
    /// Create a new `SignalStream`.
    pub fn new(signal: Signal) -> Self {
        Self(signal)
    }
}
impl Stream for SignalStream {
    type Item = ();

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Option<()>> {
        self.0.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use addr::{TargetAddr, TargetIpAddr};
    use assert_matches::assert_matches;
    use ffx_config::{ConfigLevel, test_init};
    use std::net::{IpAddr, SocketAddr};
    use tempfile::{TempDir, tempdir};

    struct TestHarness {
        context: EnvironmentContext,
        _tmp_dir: TempDir,
        process_manager: Option<MockProcessManager>,
        discovery_runner: Option<MockDiscoveryRunner>,
    }

    impl TestHarness {
        async fn setup() -> Self {
            let tmp_dir = tempdir().unwrap();
            let cache_path = tmp_dir.path().to_path_buf();
            let test_env = test_init().unwrap();
            test_env
                .context
                .query("discovery.cache_dir")
                .level(Some(ConfigLevel::User))
                .build()
                .set(&test_env.context, serde_json::to_value(cache_path.to_str().unwrap()).unwrap())
                .unwrap();
            let process_manager = Some(MockProcessManager::new());
            let discovery_runner = Some(MockDiscoveryRunner::new());
            Self {
                context: test_env.context.clone(),
                _tmp_dir: tmp_dir,
                process_manager,
                discovery_runner,
            }
        }

        fn create_discoverer(
            &mut self,
            foreground: bool,
        ) -> Discoverer<MockProcessManager, MockDiscoveryRunner> {
            Discoverer::new_with_runner(
                self.context.clone(),
                foreground,
                self.process_manager.take().unwrap(),
                self.discovery_runner.take().unwrap(),
            )
            .unwrap()
        }
    }

    #[derive(Clone)]
    struct MockDiscoveryRunner {
        call_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        fail_on_call: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl MockDiscoveryRunner {
        fn new() -> Self {
            Self {
                call_count: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                fail_on_call: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }
        }
        fn get_call_count(&self) -> usize {
            self.call_count.load(std::sync::atomic::Ordering::Relaxed)
        }
        fn set_fail_on_call(&self, fail: bool) {
            self.fail_on_call.store(fail, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[async_trait(?Send)]
    impl DiscoveryRunner for MockDiscoveryRunner {
        async fn run_discovery(&self) -> Result<()> {
            self.call_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if self.fail_on_call.load(std::sync::atomic::Ordering::Relaxed) {
                Err(anyhow!("injected error").into())
            } else {
                Ok(())
            }
        }
    }

    // Tests that target state is correctly formatted into a string.
    #[fuchsia::test]
    fn test_format_state() {
        let state = discovery::TargetState::Unknown;
        assert_eq!(format_state(state), "unknown");
        let state = discovery::TargetState::Zedboot;
        assert_eq!(format_state(state), "zedboot");

        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let socket_addr = SocketAddr::new(ip, 8080);
        let tip = TargetIpAddr::from(socket_addr);
        let state = discovery::TargetState::Product {
            addrs: vec![TargetAddr::from(tip)],
            serial: Some("1234".to_string()),
        };
        assert_eq!(format_state(state), "127.0.0.1, serial:1234 (product)");

        let state =
            discovery::TargetState::Product { addrs: vec![TargetAddr::from(tip)], serial: None };
        assert_eq!(format_state(state), "127.0.0.1 (product)");

        let fastboot_state = discovery::FastbootTargetState {
            serial_number: "1234".to_string(),
            connection_state: discovery::FastbootConnectionState::Usb,
        };
        let state = discovery::TargetState::Fastboot(fastboot_state);
        assert_eq!(format_state(state), "serial:1234, usb (fastboot)");
    }

    // Tests that the discoverer creates the cache directory if it doesn't exist.
    #[fuchsia::test]
    async fn test_discoverer_new_creates_dir() {
        let harness = TestHarness::setup().await;
        let cache_dir = ffx_target::get_discovery_cache_dir(&harness.context).unwrap();
        assert!(!cache_dir.exists());
        let _discoverer =
            Discoverer::new(harness.context, false, false, SystemProcessManager).unwrap();
        assert!(cache_dir.exists());
    }

    mod command_logic {
        use super::*;

        // Tests that running with a zero-second interval fails in the background.
        #[fuchsia::test]
        async fn test_run_once_errors_in_background() {
            let mut harness = TestHarness::setup().await;
            let mut discoverer = harness.create_discoverer(false);
            let cmd =
                DiscoverCommand { foreground: false, quiet: false, time: Some(0), stop: false };
            let result = discoverer.discover(cmd).await;
            assert_matches!(result, Err(fho::Error::User(_)));
        }

        // Tests the main "discover --stop" command logic.
        #[fuchsia::test]
        async fn test_discover_stop() {
            let mut harness = TestHarness::setup().await;
            harness.process_manager.as_mut().unwrap().expect_is_running().returning(|_| true);
            let mut discoverer = harness.create_discoverer(false);
            let cmd = DiscoverCommand { foreground: false, quiet: false, time: None, stop: true };
            let result = discoverer.discover(cmd).await;
            assert!(result.is_ok());
        }

        // Tests the main "discover --time=0" command logic.
        #[fuchsia::test]
        async fn test_discover_run_once() {
            let mut harness = TestHarness::setup().await;
            let discovery_runner = harness.discovery_runner.as_ref().unwrap().clone();
            let mut discoverer = harness.create_discoverer(true);
            let cmd =
                DiscoverCommand { foreground: true, quiet: false, time: Some(0), stop: false };
            let result = discoverer.discover(cmd).await;
            assert!(result.is_ok());
            assert_eq!(discovery_runner.get_call_count(), 1);
        }

        // Tests that the discoverer can be started as a background daemon.
        #[fuchsia::test]
        async fn test_discover_background() {
            let mut harness = TestHarness::setup().await;
            // We expect `daemonize` to be called, but we make it return an error.
            // This is a control-flow mechanism to test that the daemonization path is
            // taken without actually forking and hanging the test. The error forces
            // the `discover` function to terminate early, allowing us to assert
            // that the correct path was taken.
            harness
                .process_manager
                .as_mut()
                .unwrap()
                .expect_daemonize()
                .returning(|| Err(fho::Error::Unexpected(anyhow!("exit loop"))));
            harness.process_manager.as_mut().unwrap().expect_get_pid().returning(|| 123);
            let mut discoverer = harness.create_discoverer(false);
            let cmd = DiscoverCommand { foreground: false, quiet: false, time: None, stop: false };
            let result = discoverer.discover(cmd).await;
            // We assert that the function returned the error we injected, confirming
            // our mock was called.
            assert!(result.is_err());
        }

        // Tests that the cache file is removed.
        #[fuchsia::test]
        async fn test_remove_cache_file() {
            let mut harness = TestHarness::setup().await;
            let discoverer = harness.create_discoverer(false);
            let cache_file_path = discoverer.cache_file_path();
            fs::write(&cache_file_path, "test").unwrap();
            assert!(cache_file_path.exists());
            assert!(discoverer.remove_cache_file().is_ok());
            assert!(!cache_file_path.exists());
        }
    }

    mod stop_process {
        use super::*;

        // Tests that stopping the process succeeds when no PID file is present.
        #[fuchsia::test]
        async fn test_stop_no_pid_file() {
            let mut harness = TestHarness::setup().await;
            let discoverer = harness.create_discoverer(false);
            assert!(discoverer.stop_process().is_ok());
        }

        // Tests that `stop_process` removes a stale PID file.
        #[fuchsia::test]
        async fn test_stop_process_removes_stale_pid() {
            let mut harness = TestHarness::setup().await;
            harness.process_manager.as_mut().unwrap().expect_is_running().returning(|_| false);
            let discoverer = harness.create_discoverer(false);
            fs::write(&discoverer.pid_path, "123").unwrap();
            assert!(discoverer.stop_process().is_ok());
            assert!(!discoverer.pid_path.exists());
        }

        // Tests that `stop_process` removes the PID file for a running process.
        #[fuchsia::test]
        async fn test_stop_process_running() {
            let mut harness = TestHarness::setup().await;
            harness.process_manager.as_mut().unwrap().expect_is_running().returning(|_| true);
            let discoverer = harness.create_discoverer(false);
            fs::write(&discoverer.pid_path, "123").unwrap();
            assert!(discoverer.stop_process().is_ok());
            assert!(!discoverer.pid_path.exists());
        }

        // Tests that `stop_process` handles and removes a corrupt PID file.
        #[fuchsia::test]
        async fn test_stop_process_corrupt_pid() {
            let mut harness = TestHarness::setup().await;
            let discoverer = harness.create_discoverer(false);
            fs::write(&discoverer.pid_path, "not-a-pid").unwrap();
            assert!(discoverer.stop_process().is_ok());
            assert!(!discoverer.pid_path.exists());
        }
    }

    mod process_management {
        use super::*;

        // Tests that starting in the foreground fails if a background process is already running.
        #[fuchsia::test]
        async fn test_do_process_management_running_foreground() {
            let mut harness = TestHarness::setup().await;
            harness.process_manager.as_mut().unwrap().expect_is_running().returning(|_| true);
            let mut discoverer = harness.create_discoverer(true);
            fs::write(&discoverer.pid_path, "123").unwrap();
            let result = discoverer.do_process_management().await;
            assert_matches!(result, Err(fho::Error::User(_)));
        }

        // Tests that signaling a running background process works correctly.
        #[fuchsia::test]
        async fn test_do_process_management_running_background() {
            let mut harness = TestHarness::setup().await;
            harness.process_manager.as_mut().unwrap().expect_is_running().returning(|_| true);
            harness.process_manager.as_mut().unwrap().expect_kill_pid().returning(|_, _| Ok(()));
            let mut discoverer = harness.create_discoverer(false);
            fs::write(&discoverer.pid_path, "123").unwrap();
            let fut = discoverer.do_process_management();
            let cache_dir = ffx_target::get_discovery_cache_dir(&harness.context).unwrap();
            let mut cache_path = cache_dir.clone();
            cache_path.push(discovery::CACHE_FILE_NAME);
            let (result, ()) = futures::future::join(fut, async {
                // Give the watcher time to set up
                Timer::new(std::time::Duration::from_millis(200)).await;
                fs::write(cache_path, "test").unwrap();
            })
            .await;
            assert!(result.is_ok());
        }

        // Tests that `do_process_management` handles a corrupt PID file.
        #[fuchsia::test]
        async fn test_do_process_management_corrupt_pid() {
            let mut harness = TestHarness::setup().await;
            let mut discoverer = harness.create_discoverer(false);
            fs::write(&discoverer.pid_path, "not-a-pid").unwrap();
            let result = discoverer.do_process_management().await;
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), true);
            assert!(!discoverer.pid_path.exists());
        }

        // Tests that `do_process_management` handles a failure to signal the background process.
        #[fuchsia::test]
        async fn test_do_process_management_signal_fails() {
            let mut harness = TestHarness::setup().await;
            harness.process_manager.as_mut().unwrap().expect_is_running().returning(|_| true);
            harness
                .process_manager
                .as_mut()
                .unwrap()
                .expect_kill_pid()
                .returning(|_, _| Err(nix::Error::from_raw(nix::errno::Errno::EPERM as i32)));
            let mut discoverer = harness.create_discoverer(false);
            fs::write(&discoverer.pid_path, "123").unwrap();
            let result = discoverer.do_process_management().await;
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), true);
        }

        // Tests that `do_process_management` recovers from a stale PID by starting a new daemon.
        #[fuchsia::test]
        async fn test_do_process_management_recovers_from_stale_pid() {
            let mut harness = TestHarness::setup().await;
            harness.process_manager.as_mut().unwrap().expect_is_running().returning(|_| false);
            harness.process_manager.as_mut().unwrap().expect_daemonize().returning(|| Ok(()));
            harness.process_manager.as_mut().unwrap().expect_get_pid().returning(|| 456);

            let mut discoverer = harness.create_discoverer(false);
            fs::write(&discoverer.pid_path, "123").unwrap(); // Stale PID
            let result = discoverer.do_process_management().await;
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), false); // Should proceed to run_loop
        }

        // Tests that `wait_for_new_cache` will hang indefinitely if the cache is not updated.
        #[fuchsia::test]
        async fn test_wait_for_new_cache_times_out() {
            let mut harness = TestHarness::setup().await;
            harness.process_manager.as_mut().unwrap().expect_is_running().returning(|_| true);
            harness.process_manager.as_mut().unwrap().expect_kill_pid().returning(|_, _| Ok(()));
            let mut discoverer = harness.create_discoverer(false);
            fs::write(&discoverer.pid_path, "123").unwrap();

            let fut = discoverer.do_process_management();
            let timeout = Timer::new(Duration::from_millis(200));

            futures::select! {
                _ = fut.fuse() => {
                    panic!("do_process_management returned unexpectedly, it should have hung");
                },
                _ = timeout.fuse() => {
                    // This is the expected outcome. The timeout fired.
                }
            }
        }
    }

    mod run_loop {
        use super::*;

        // Tests that the main run loop exits when its PID file is deleted.
        #[fuchsia::test]
        async fn test_run_loop_exits_on_pid_delete() {
            let mut harness = TestHarness::setup().await;
            harness.process_manager.as_mut().unwrap().expect_get_pid().returning(|| 123);
            let discoverer = harness.create_discoverer(true);
            discoverer.write_pid().unwrap();

            let mut pending_stream = futures::stream::pending();
            let loop_fut = discoverer.run_loop(Duration::from_secs(1), &mut pending_stream);

            // Create a separate future for the deletion logic
            let delete_fut = async {
                // Give the loop a moment to start up and create the watcher
                Timer::new(Duration::from_millis(200)).await;
                fs::remove_file(&discoverer.pid_path).unwrap();
            };

            // Run both futures concurrently. join! will complete when both are done.
            let (loop_res, _) = futures::join!(loop_fut, delete_fut);
            assert!(loop_res.is_ok());

            // By the time we get here, the loop should have exited immediately
            // without running discovery.
            assert_eq!(discoverer.discovery_runner.get_call_count(), 0);
        }

        // Tests that the run loop's timer correctly triggers discovery multiple times.
        #[fuchsia::test]
        async fn test_run_loop_timer() {
            let mut harness = TestHarness::setup().await;
            harness.process_manager.as_mut().unwrap().expect_get_pid().returning(|| 123);
            let discoverer = harness.create_discoverer(true);
            discoverer.write_pid().unwrap();
            let mut pending_stream = futures::stream::pending();
            let loop_fut = discoverer.run_loop(Duration::from_millis(100), &mut pending_stream);
            let timeout_fut = Timer::new(Duration::from_secs(1));
            futures::select! {
                res = loop_fut.fuse() => { assert!(res.is_ok()) },
                _ = timeout_fut.fuse() => {
                    assert!(discoverer.discovery_runner.get_call_count() > 2);
                    fs::remove_file(&discoverer.pid_path).unwrap();
                },
            }
        }

        // Tests that the run loop correctly triggers discovery upon receiving a SIGUSR1 signal.
        #[fuchsia::test]
        async fn test_run_loop_rediscovered_on_signal() {
            let mut harness = TestHarness::setup().await;
            let discovery_runner = harness.discovery_runner.as_ref().unwrap().clone();
            harness.process_manager.as_mut().unwrap().expect_get_pid().returning(|| 123);
            let discoverer = harness.create_discoverer(true);
            discoverer.write_pid().unwrap();

            // We use a channel to simulate a signal being sent to the discoverer,
            // since we don't want to send a real SIGUSR1 to the test process.
            let (mut tx, mut rx) = mpsc::channel(1);

            let loop_fut = discoverer.run_loop(Duration::from_secs(1000), &mut rx);

            let test_fut = async {
                // Give the loop a moment to start up and enter the select! macro.
                Timer::new(Duration::from_millis(200)).await;
                assert_eq!(discovery_runner.get_call_count(), 0);

                // Send a signal to the loop.
                tx.send(()).await.unwrap();

                // Give the loop a moment to process the signal.
                Timer::new(Duration::from_millis(200)).await;
                assert_eq!(discovery_runner.get_call_count(), 1);

                // Clean up by removing the pid file, which will cause the loop to exit.
                fs::remove_file(&discoverer.pid_path).unwrap();
            };

            let (loop_res, _) = futures::join!(loop_fut, test_fut);
            assert!(loop_res.is_ok());
        }

        // Tests that the main run loop exits when discovery returns an error.
        #[fuchsia::test]
        async fn test_run_loop_exits_on_discovery_error() {
            let mut harness = TestHarness::setup().await;
            let discovery_runner = harness.discovery_runner.as_ref().unwrap().clone();
            harness.process_manager.as_mut().unwrap().expect_get_pid().returning(|| 123);
            let discoverer = harness.create_discoverer(true);
            discoverer.write_pid().unwrap();
            discovery_runner.set_fail_on_call(true);

            let mut pending_stream = futures::stream::pending();
            let result = discoverer.run_loop(Duration::from_millis(1), &mut pending_stream).await;

            assert_matches!(result, Err(_));
            assert_eq!(discovery_runner.get_call_count(), 1);
        }
    }
}
