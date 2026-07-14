// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::doctor_ledger::*;
use crate::ledger_view::*;
use anyhow::{Context, Result};
use async_lock::Mutex;
use async_trait::async_trait;
use doctor_utils::{DaemonManager, DefaultDaemonManager, DoctorRecorder};
use errors::{ffx_bail, ffx_error};
use ffx_build_version::VersionInfo;
use ffx_config::EnvironmentContext;
use ffx_daemon::DaemonConfig;
use ffx_doctor_args::DoctorCommand;
use ffx_ssh::SshKeyFiles;
use ffx_target::get_target_specifier;
use ffx_target_show::ShowTool;
use ffx_target_show_args::TargetShow;
use ffx_writer::{MachineWriter, ToolIO, VerifiedMachineWriter};
use fho::{FfxMain, FfxTool, FhoEnvironment};
use std::io::{Write, stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use termio::Colors;

mod daemon;
mod doctor_ledger;
mod environment;
mod gcheck;
mod ledger_view;
mod network;
mod record;
mod single_target_diagnostics;
mod target;
mod types;
mod usb;

use crate::daemon::{check_daemon_status, doctor_daemon_restart};
use crate::environment::{
    check_emulators, check_env_context, check_ffx_info, check_inotify_watches,
    get_config_permission,
};
use crate::network::run_google_network_checks;
use crate::record::doctor_record;
use crate::target::{check_targets_locally, check_targets_via_daemon};
use crate::types::{
    DefaultDoctorStepHandler, DoctorRecorderParameters, DoctorResult, DoctorStepHandler, StepType,
};
use crate::usb::{CommandUsbDriverFinder, UsbDriverFinder, check_usb_driver};

pub struct ShowToolWrapper {
    env: FhoEnvironment,
    inner: Option<ShowTool>,
}

impl ShowToolWrapper {
    async fn allocate(&mut self, target_spec: Option<String>) -> fho::Result<()> {
        let mut context = self
            .env
            .ffx_command()
            .global
            .load_context(self.env.environment_context().exe_kind())?;
        context.override_target_specifier(&target_spec);
        let fho_env = FhoEnvironment::new(&context, self.env.ffx_command());
        self.inner.replace(ShowTool::from_env(fho_env, TargetShow::default()).await?);
        Ok(())
    }

    /// This requires that `allocate` is run first. This is really only to ensure that there are
    /// two steps in the process for running an invocation of `ffx target show`.
    async fn run(&mut self) -> fho::Result<(String, String)> {
        let tool = self.inner.take().unwrap();
        let buffers = ffx_writer::TestBuffers::default();
        match tool.main(VerifiedMachineWriter::new_test(None, &buffers)).await {
            Ok(_) => Ok(buffers.into_strings()),
            Err(e) => Err(fho::user_error!("{}\n\tstderr: {}", e, buffers.into_stderr_str())),
        }
    }
}

#[async_trait(?Send)]
impl fho::TryFromEnv for ShowToolWrapper {
    type Error = std::convert::Infallible;
    async fn try_from_env(env: &FhoEnvironment) -> std::result::Result<Self, Self::Error> {
        Ok(Self { env: env.clone(), inner: None })
    }
}

#[derive(FfxTool)]
pub struct DoctorTool {
    #[command]
    cmd: DoctorCommand,
    show_tool: ShowToolWrapper,
    context: EnvironmentContext,
}

fho::embedded_plugin!(DoctorTool);

#[async_trait(?Send)]
impl FfxMain for DoctorTool {
    type Writer = MachineWriter<DoctorResult>;

    type Error = ::fho::Error;

    async fn main(self, mut writer: Self::Writer) -> fho::Result<()> {
        // TODO(b/373720502): This is passing a `Some(self.show_tool)` to make it simpler not to
        // have to update existing tests that take in a dozen arguments. The proper approach for
        // this is to refactor `ffx doctor` to make testing things like this less cumbersome.
        // TODO(b/373723080): Add actual tests for the usage of `ffx target show` within `ffx
        // doctor`.
        // This duplication avoids dynamic dispatch overhead from generic Write arguments
        // (std::io::Sink vs std::io::Stdout), which would result in different types.
        if writer.is_machine() {
            let ledger = Box::pin(doctor_cmd_impl(
                self.context,
                self.cmd,
                Some(self.show_tool),
                std::io::sink(),
                std::io::sink(),
            ))
            .await?;
            writer.machine(&DoctorResult { steps: ledger.into_root_node() })?;
        } else {
            Box::pin(doctor_cmd_impl(
                self.context,
                self.cmd,
                Some(self.show_tool),
                stdout(),
                stdout(),
            ))
            .await?;
        }
        Ok(())
    }
}

pub async fn doctor_cmd_impl<
    StepWriter: Write + Send + Sync + 'static,
    LedgerWriter: Write + Send + Sync + 'static,
>(
    context: EnvironmentContext,

    mut cmd: DoctorCommand,
    show_tool: Option<ShowToolWrapper>,
    step_writer: StepWriter,
    ledger_writer: LedgerWriter,
) -> Result<DoctorLedger<LedgerWriter>> {
    let mut writer: Box<dyn Write + Send + Sync + 'static> = Box::new(step_writer);
    let gchecker = gcheck::DefaultGChecker;
    let node = overnet_core::Router::new(None)
        .with_context(|| ffx_error!("Could not initialize Overnet"))?;
    let ascendd_path = context.get_ascendd_path().await?;
    let daemon_manager = DefaultDaemonManager::new(context.clone(), node, ascendd_path);
    let delay = Duration::from_millis(cmd.retry_delay);
    let target_spec = ffx_target::get_target_specifier(&context)?;
    let target_str = target_spec.unwrap_or_else(String::default);
    let version_info: VersionInfo = context.build_info();
    let colors = Colors::current();
    let mut log_root = None;
    let mut output_dir = None;
    let mut record = cmd.record;
    // Force-enable verbose mode if `record` is enabled.
    if record {
        cmd.verbose = true;
    }
    match context.get("log.enabled") {
        Ok(enabled) => {
            let enabled: bool = enabled;
            if !enabled && cmd.record {
                writeln!(
                    &mut writer,
                    "{}WARNING:{} --record was provided but ffx logs are not enabled. This means your record will only include doctor output.",
                    colors.red, colors.reset
                )?;
                writeln!(
                    &mut writer,
                    "ffx doctor will proceed, but if you want to enable logs, you can do so by running:"
                )?;
                writeln!(&mut writer, "  ffx config set log.enabled true")?;
                writeln!(&mut writer, "You will then need to restart the ffx daemon:")?;
                writeln!(&mut writer, "  ffx doctor --force-restart\n\n")?;
                fuchsia_async::Timer::new(Duration::from_millis(10000)).await;
            }

            log_root = Some(context.get("log.dir")?);
            let final_output_dir =
                cmd.output_dir.map(|s| PathBuf::from(s)).unwrap_or(std::env::current_dir()?);

            if !final_output_dir.is_dir() {
                ffx_bail!(
                    "cannot record: output directory does not exist or is unreadable: {:?}",
                    output_dir
                );
            }

            output_dir = Some(final_output_dir);
        }
        Err(e) => {
            writeln!(
                &mut writer,
                "{}WARNING:{} getting log status from ffx config failed. The error was: {:?}",
                colors.red, colors.reset, e
            )?;
            if cmd.record {
                writeln!(
                    &mut writer,
                    "Record mode requires configuration and will be turned off for this run."
                )?;
            }
            writeln!(
                &mut writer,
                "If this issue persists, please file a bug here: {}",
                errors::BUG_REPORT_URL
            )?;
            fuchsia_async::Timer::new(Duration::from_millis(10000)).await;

            record = false;
        }
    };

    let user_config_enabled = if !record || cmd.no_config {
        false
    } else {
        match get_config_permission(&context, &mut writer).await {
            Ok(b) => b,
            Err(e) => {
                writeln!(&mut writer, "Failed to get permission to record config data: {}", e)?;
                writeln!(&mut writer, "Config data will not be recorded")?;
                false
            }
        }
    };

    if cmd.repair_keys {
        let keys = SshKeyFiles::load(&context)?;
        let message = keys.check_keys(true)?;
        writeln!(&mut writer, "{message}")?;
    }

    let recorder = Arc::new(Mutex::new(DoctorRecorder::new()));
    let mut handler = DefaultDoctorStepHandler::new(recorder.clone(), writer, colors);
    let target_spec =
        get_target_specifier(&context).map_err(|e| format!("{:?}", e).replace("\n", ""));

    // create ledger
    let ledger_mode = match cmd.verbose {
        true => LedgerViewMode::Verbose,
        false => LedgerViewMode::Normal,
    };
    let mut ledger =
        DoctorLedger::new(ledger_writer, Box::new(VisualLedgerView::new()), ledger_mode);
    let usb_driver_finder = CommandUsbDriverFinder {};

    doctor(
        &mut handler,
        &mut ledger.root_guard(),
        context.get_direct_connection_mode(),
        &daemon_manager,
        &target_str,
        cmd.retry_count,
        delay,
        cmd.restart_daemon,
        version_info,
        target_spec,
        &context,
        DoctorRecorderParameters {
            record,
            user_config_enabled,
            log_root,
            output_dir,
            recorder: recorder.clone(),
        },
        usb_driver_finder,
        gchecker,
        show_tool,
        true,
    )
    .await?;

    match ledger.calc_outcome(0) {
        LedgerOutcome::Warning => {
            handler.output_step(StepType::DoctorNoticeWarning).await?;
        }
        LedgerOutcome::Failure => {
            handler.output_step(StepType::DoctorNoticeFailure).await?;
        }
        _ => {}
    }

    Ok(ledger)
}

async fn doctor<W: Write>(
    step_handler: &mut impl DoctorStepHandler,
    ledger: &mut LedgerNodeGuard<'_, W>,
    direct_mode: bool,
    daemon_manager: &impl DaemonManager,
    target_str: &str,
    _retry_count: usize,
    retry_delay: Duration,
    restart_daemon: bool,
    version_info: VersionInfo,
    target_spec: Result<Option<String>, String>,
    env_context: &EnvironmentContext,
    record_params: DoctorRecorderParameters,
    usb_driver_finder: impl UsbDriverFinder,
    gchecker: impl gcheck::GChecker,
    show_tool: Option<ShowToolWrapper>,
    run_additional_diagnostics: bool,
) -> Result<()> {
    if restart_daemon {
        doctor_daemon_restart(daemon_manager, retry_delay, ledger).await;
    }

    doctor_summary(
        step_handler,
        direct_mode,
        daemon_manager,
        target_str,
        retry_delay,
        version_info,
        target_spec,
        env_context,
        show_tool,
        run_additional_diagnostics,
        usb_driver_finder,
        gchecker,
        ledger,
    )
    .await?;

    if record_params.record {
        let mut record_view = RecordLedgerView::new();
        let data = ledger.write_all(&mut record_view);
        step_handler.record(StepType::Output(data)).await?;
        doctor_record(env_context, step_handler, record_params).await?;
    }

    Ok(())
}

fn print_summary_outcome<W: Write>(ledger: &mut LedgerNodeGuard<'_, W>) {
    match ledger.calc_outcome_at_next_depth() {
        LedgerOutcome::Failure => {
            let msg = match ledger.get_ledger_mode() {
                LedgerViewMode::Normal => String::from(
                    "Doctor found issues in one or more categories; \
                    run 'ffx doctor -v' for more details.",
                ),
                _ => String::from("Doctor found issues in one or more categories."),
            };
            let node = ledger.add_node(&msg, LedgerMode::Automatic);
            node.set_outcome(LedgerOutcome::Failure);
        }
        _ => {
            let node = ledger.add_node("No issues found", LedgerMode::Automatic);
            node.set_outcome(LedgerOutcome::Success);
        }
    }
}

async fn doctor_summary<W: Write>(
    step_handler: &mut impl DoctorStepHandler,
    direct_mode: bool,
    daemon_manager: &impl DaemonManager,
    target_str: &str,
    retry_delay: Duration,
    version_info: VersionInfo,
    target_spec: Result<Option<String>, String>,
    env_context: &EnvironmentContext,
    show_tool: Option<ShowToolWrapper>,
    run_additional_diagnostics: bool,
    usb_driver_finder: impl UsbDriverFinder,
    gchecker: impl gcheck::GChecker,
    ledger: &mut LedgerNodeGuard<'_, W>,
) -> Result<()> {
    match ledger.get_ledger_mode() {
        LedgerViewMode::Normal => {
            step_handler.output_step(StepType::DoctorSummaryInitNormal).await?
        }
        LedgerViewMode::Verbose => {
            step_handler.output_step(StepType::DoctorSummaryInitVerbose).await?
        }
    }

    check_ffx_info(ledger, &version_info).await;
    check_env_context(ledger, env_context).await?;
    check_emulators(ledger, env_context).await?;
    check_inotify_watches(ledger).await;

    // Even in direct mode, we might as well at least report the status of the daemon.
    let daemon_proxy = check_daemon_status(
        ledger,
        direct_mode,
        daemon_manager,
        retry_delay,
        &version_info,
        &target_spec,
    )
    .await?;

    check_usb_driver(&usb_driver_finder, ledger, env_context).await;

    run_google_network_checks(ledger, env_context, &gchecker).await?;
    if direct_mode {
        check_targets_locally(ledger, target_str, env_context, show_tool, retry_delay).await?;
    } else {
        if let Some(daemon_proxy) = daemon_proxy {
            check_targets_via_daemon(
                ledger,
                target_str,
                retry_delay,
                env_context,
                show_tool,
                run_additional_diagnostics,
                &daemon_proxy,
            )
            .await?;
        }
    }

    print_summary_outcome(ledger);

    Ok(())
}

///////////////////////////////////////////////////////////////////////////////////////////////////
// Tests
///////////////////////////////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod test {
    use super::*;
    use crate::record::collect_log_files;
    use crate::types::StepResult;
    use crate::usb::{FindUsbDriverError, MockUsbDriverFinder, UsbDriverStatus};
    use async_trait::async_trait;
    use doctor_utils::Recorder;
    use emulator_instance::{EmulatorInstanceData, EngineState};
    use ffx_config::TestEnv;
    use ffx_doctor_test_utils::MockWriter;
    use fidl::Channel;
    use fidl::endpoints::{ProtocolMarker, Request, RequestStream, ServerEnd};
    use fidl_fuchsia_developer_ffx::{
        DaemonProxy, DaemonRequest, OpenTargetError, RemoteControlState, TargetCollectionRequest,
        TargetCollectionRequestStream, TargetConnectionError, TargetInfo, TargetMarker,
        TargetQuery, TargetRequest, TargetState,
    };
    use fidl_fuchsia_developer_remotecontrol::{
        IdentifyHostResponse, RemoteControlMarker, RemoteControlRequest,
    };
    use fidl_test_util::spawn_local_stream_handler;
    use fuchsia_async as fasync;
    use futures::channel::oneshot::{self, Receiver};
    use futures::future::Shared;
    use futures::{Future, FutureExt, TryFutureExt, TryStreamExt};
    use mockall::predicate::*;
    use pretty_assertions::{Comparison, assert_eq};
    use serde_json::json;
    use std::cell::Cell;
    use std::collections::HashSet;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::{fmt, fs};
    use tempfile::tempdir;

    const NODENAME: &str = "fake-nodename";
    const UNRESPONSIVE_NODENAME: &str = "fake-nodename-unresponsive";
    const SSH_ERR_NODENAME: &str = "fake-nodename-ssh-error";
    const FASTBOOT_NODENAME: &str = "fastboot-nodename-unresponsive";
    const NON_EXISTENT_NODENAME: &str = "extra-fake-nodename";
    const SERIAL_NUMBER: &str = "123123123";
    const DEFAULT_RETRY_DELAY: Duration = Duration::from_millis(2000);
    const DAEMON_VERSION: &str = "daemon-build-string";
    const FRONTEND_VERSION: &str = "fake version";
    const INDENT_STR: &str = "    ";
    const FAKE_ABI_REVISION: u64 = 17063755220075245312;
    const ABI_REVISION_STR: &str = "0xECCEA2F70ACD6F00";
    const FAKE_API_LEVEL: u64 = 7;
    const ANOTHER_FAKE_API_LEVEL: u64 = 8;

    struct FakeGChecker;

    impl gcheck::GChecker for FakeGChecker {
        fn is_gcorp_machine(&self) -> bool {
            true
        }
    }

    #[derive(PartialEq)]
    struct TestStep {
        step_type: StepType,
        output_only: bool,
    }

    impl std::fmt::Debug for TestStep {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let output_str = if self.output_only { " (output)" } else { "" };

            write!(f, "{:?}{}", self.step_type, output_str)
        }
    }

    struct TestStepEntry {
        step: Option<TestStep>,
        result: Option<StepResult>,
    }

    impl TestStepEntry {
        fn step(step_type: StepType) -> Self {
            Self { step: Some(TestStep { step_type, output_only: false }), result: None }
        }

        fn output_step(step_type: StepType) -> Self {
            Self { step: Some(TestStep { step_type, output_only: true }), result: None }
        }

        fn result(result: StepResult) -> Self {
            Self { result: Some(result), step: None }
        }
    }

    impl std::fmt::Debug for TestStepEntry {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            if self.step.is_some() {
                write!(f, "{:?}", self.step.as_ref().unwrap())
            } else if self.result.is_some() {
                write!(f, "{:?}", self.result.as_ref().unwrap())
            } else {
                panic!("attempted to debug TestStepEntry with empty step and result")
            }
        }
    }

    impl PartialEq for TestStepEntry {
        fn eq(&self, other: &Self) -> bool {
            if self.step != other.step {
                return false;
            }

            match (self.result.as_ref(), other.result.as_ref()) {
                (Some(r), Some(r2)) => match (r, r2) {
                    (StepResult::Success, StepResult::Success) => true,
                },
                (None, None) => true,
                _ => false,
            }
        }
    }

    struct FakeLedgerView {
        tree: LedgerViewNode,
        omit_error_reason: bool,
    }

    impl FakeLedgerView {
        pub fn new() -> Self {
            FakeLedgerView { tree: LedgerViewNode::default(), omit_error_reason: true }
        }
        pub fn new_with_error_reason() -> Self {
            FakeLedgerView { tree: LedgerViewNode::default(), omit_error_reason: false }
        }
        fn gen_output(&self, parent_node: &LedgerViewNode, indent_level: usize) -> String {
            let mut data = parent_node.data.clone();
            // Remove error details to make the tests more stable
            if self.omit_error_reason && data.starts_with("Error") {
                let v: Vec<_> = data.split(":").collect();
                if v.len() > 1 {
                    data = format!("{}: <reason omitted>", v.first().unwrap().to_string());
                }
            }

            let mut output_str = format!(
                "{}[{}] {}\n",
                INDENT_STR.repeat(indent_level),
                parent_node.outcome.format(&Colors::disabled()),
                data
            );

            for child_node in &parent_node.children {
                let child_str = self.gen_output(child_node, indent_level + 1);
                output_str = format!("{}{}", output_str, child_str);
            }

            return output_str;
        }
    }

    impl fmt::Display for FakeLedgerView {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}", self.gen_output(&self.tree, 0))
        }
    }

    impl LedgerView for FakeLedgerView {
        fn set(&mut self, new_tree: LedgerViewNode) {
            self.tree = new_tree;
        }
    }

    struct FakeStepHandler {
        steps: Arc<Mutex<Vec<TestStepEntry>>>,
    }
    impl FakeStepHandler {
        fn new() -> Self {
            Self { steps: Arc::new(Mutex::new(Vec::new())) }
        }

        async fn assert_matches_steps(&self, expected_steps: Vec<TestStepEntry>) {
            let steps = self.steps.lock().await;
            if *steps != expected_steps {
                let comparison = Comparison::new(&steps, &expected_steps);
                println!("steps differ: {}", comparison);

                for (step, expected) in steps.iter().zip(expected_steps) {
                    if *step != expected {
                        let step_comparison = Comparison::new(&step, &expected);
                        println!("different step: {}", step_comparison);
                    }
                }
                panic!("steps didn't match. differences are listed above.");
            }
        }
    }

    #[async_trait]
    impl DoctorStepHandler for FakeStepHandler {
        async fn step(&mut self, step: StepType) -> Result<()> {
            let mut v = self.steps.lock().await;
            v.push(TestStepEntry::step(step));
            Ok(())
        }

        async fn output_step(&mut self, step: StepType) -> Result<()> {
            let mut v = self.steps.lock().await;
            v.push(TestStepEntry::output_step(step));
            Ok(())
        }

        async fn record(&mut self, step: StepType) -> Result<()> {
            let mut v = self.steps.lock().await;
            v.push(TestStepEntry::output_step(step));
            Ok(())
        }

        async fn result(&mut self, result: StepResult) -> Result<()> {
            let mut v = self.steps.lock().await;
            v.push(TestStepEntry::result(result));
            Ok(())
        }
    }

    struct FakeRecorder {
        expected_sources: Vec<PathBuf>,
        expected_output_dir: PathBuf,
        generate_called: Cell<bool>,
    }

    impl FakeRecorder {
        fn new(expected_sources: Vec<PathBuf>, expected_output_dir: PathBuf) -> Self {
            return Self {
                expected_sources,
                expected_output_dir,
                generate_called: Cell::new(false),
            };
        }

        fn assert_generate_called(&self) {
            assert!(self.generate_called.get())
        }

        fn result_path() -> PathBuf {
            PathBuf::from("/tmp").canonicalize().unwrap()
        }
    }

    impl Recorder for FakeRecorder {
        fn add_sources(&mut self, sources: Vec<PathBuf>) {
            let source_set: HashSet<_> = sources.iter().collect();
            let expected_set: HashSet<_> = self.expected_sources.iter().collect();
            assert_eq!(source_set, expected_set);
        }

        fn add_content(&mut self, _filename: &str, _content: String) {
            // Do nothing, we don't verify output in tests.
        }

        fn generate(&self, output_dir: PathBuf) -> Result<PathBuf, doctor_utils::DoctorUtilsError> {
            assert_eq!(output_dir, self.expected_output_dir);
            self.generate_called.set(true);
            Ok(Self::result_path())
        }
    }
    struct DisabledRecorder {}

    impl DisabledRecorder {
        fn new() -> Self {
            return Self {};
        }
    }

    impl Recorder for DisabledRecorder {
        fn add_sources(&mut self, _sources: Vec<PathBuf>) {
            panic!("add_sources should not be called.")
        }

        fn add_content(&mut self, _filename: &str, _content: String) {
            // Do nothing, we don't verify output in tests.
        }

        fn generate(
            &self,
            _output_dir: PathBuf,
        ) -> Result<PathBuf, doctor_utils::DoctorUtilsError> {
            panic!("generate should not be called.")
        }
    }

    struct FakeStateManager {
        kill_results: Vec<Result<bool, doctor_utils::DoctorUtilsError>>,
        daemons_running_results: Vec<bool>,
        spawn_results: Vec<Result<(), doctor_utils::DoctorUtilsError>>,
        find_and_connect_results: Vec<Result<DaemonProxy, doctor_utils::DoctorUtilsError>>,
        get_pid_results: Vec<Result<Vec<usize>, doctor_utils::DoctorUtilsError>>,
    }

    struct FakeDaemonManager {
        state_manager: Arc<Mutex<FakeStateManager>>,
    }

    impl FakeDaemonManager {
        fn new(
            daemons_running_results: Vec<bool>,
            kill_results: Vec<Result<bool, doctor_utils::DoctorUtilsError>>,
            spawn_results: Vec<Result<(), doctor_utils::DoctorUtilsError>>,
            find_and_connect_results: Vec<Result<DaemonProxy, doctor_utils::DoctorUtilsError>>,
            get_pid_results: Vec<Result<Vec<usize>, doctor_utils::DoctorUtilsError>>,
        ) -> Self {
            return FakeDaemonManager {
                state_manager: Arc::new(Mutex::new(FakeStateManager {
                    kill_results,
                    daemons_running_results,
                    spawn_results,
                    find_and_connect_results,
                    get_pid_results,
                })),
            };
        }

        async fn assert_no_leftover_calls(&self) {
            let state = self.state_manager.lock().await;
            assert!(
                state.kill_results.is_empty(),
                "too few calls to kill_all. remaining entries: {:?}",
                state.kill_results
            );
            assert!(
                state.daemons_running_results.is_empty(),
                "too few calls to is_daemon_running. remaining entries: {:?}",
                state.daemons_running_results
            );
            assert!(
                state.spawn_results.is_empty(),
                "too few calls to spawn. remaining entries: {:?}",
                state.spawn_results
            );
            assert!(
                state.find_and_connect_results.is_empty(),
                "too few calls to find_and_connect. remaining entries: {:?}",
                state.find_and_connect_results
            );
        }
    }

    #[async_trait]
    impl DaemonManager for FakeDaemonManager {
        async fn kill_all(&self) -> Result<bool, doctor_utils::DoctorUtilsError> {
            let mut state = self.state_manager.lock().await;
            assert!(!state.kill_results.is_empty(), "too many calls to kill_all");
            state.kill_results.remove(0)
        }

        async fn get_pid(&self) -> Result<Vec<usize>, doctor_utils::DoctorUtilsError> {
            let mut state = self.state_manager.lock().await;
            assert!(!state.get_pid_results.is_empty(), "too many calls to spawn");
            state.get_pid_results.remove(0)
        }

        async fn is_daemon_running(&self) -> bool {
            let mut state = self.state_manager.lock().await;
            assert!(
                !state.daemons_running_results.is_empty(),
                "too many calls to is_daemon_running"
            );
            state.daemons_running_results.remove(0)
        }

        async fn spawn(&self) -> Result<(), doctor_utils::DoctorUtilsError> {
            let mut state = self.state_manager.lock().await;
            assert!(!state.spawn_results.is_empty(), "too many calls to spawn");
            state.spawn_results.remove(0)
        }

        async fn find_and_connect(&self) -> Result<DaemonProxy, doctor_utils::DoctorUtilsError> {
            let mut state = self.state_manager.lock().await;
            assert!(
                !state.find_and_connect_results.is_empty(),
                "too many calls to find_and_connect"
            );
            state.find_and_connect_results.remove(0)
        }
    }

    fn serve_stream<T, F, Fut>(stream: T::RequestStream, mut f: F)
    where
        T: ProtocolMarker,
        F: FnMut(Request<T>) -> Fut + 'static + std::marker::Send,
        Fut: Future<Output = ()> + 'static + std::marker::Send,
    {
        fasync::Task::local(
            stream
                .try_for_each(move |r| f(r).map(Ok))
                .unwrap_or_else(|e| panic!("failed to handle request: {:?}", e)),
        )
        .detach();
    }

    // Spawns a target collection, accepting closures for handling listing and opening target
    // handles.
    fn spawn_target_collection<F, F2>(
        server_channel: Channel,
        list_closure: F,
        open_targets_closure: F2,
    ) where
        F: Fn(TargetQuery) -> Vec<TargetInfo> + Clone + 'static,
        F2: Fn(TargetQuery, ServerEnd<TargetMarker>) -> Result<(), OpenTargetError>
            + Clone
            + 'static,
    {
        let channel = fidl::AsyncChannel::from_channel(server_channel);
        let mut stream = TargetCollectionRequestStream::from_channel(channel);
        fuchsia_async::Task::local(async move {
            while let Ok(Some(req)) = stream.try_next().await {
                match req {
                    TargetCollectionRequest::ListTargets { query, reader, .. } => {
                        let reader = reader.into_proxy();
                        let list_closure = list_closure.clone();
                        let results = (list_closure)(query);
                        if !results.is_empty() {
                            reader.next(&results).await.unwrap();
                            reader.next(&[]).await.unwrap();
                        } else {
                            reader.next(&[]).await.unwrap();
                        }
                    }
                    TargetCollectionRequest::OpenTarget { query, responder, target_handle } => {
                        let res = (open_targets_closure)(query, target_handle);
                        responder.send(res).unwrap();
                    }
                    _ => {}
                }
            }
        })
        .detach();
    }

    fn spawn_target_handler<F>(target_handle: ServerEnd<TargetMarker>, handler: F)
    where
        F: Fn(TargetRequest) -> () + 'static,
    {
        fuchsia_async::Task::local(async move {
            let mut stream = target_handle.into_stream();
            while let Ok(Some(req)) = stream.try_next().await {
                (handler)(req)
            }
        })
        .detach();
    }

    fn setup_responsive_daemon_server() -> DaemonProxy {
        spawn_local_stream_handler(move |req| async move {
            match req {
                DaemonRequest::GetVersionInfo { responder } => {
                    responder.send(&daemon_version_info()).unwrap();
                }
                DaemonRequest::ConnectToProtocol { responder, name: _, server_end } => {
                    spawn_target_collection(
                        server_end,
                        |_| vec![],
                        |_query, target_handle| {
                            spawn_target_handler(target_handle, |req| match req {
                                TargetRequest::OpenRemoteControl {
                                    responder,
                                    remote_control: _,
                                } => {
                                    responder.send(Ok(())).unwrap();
                                }
                                r => panic!("unexpected request: {:?}", r),
                            });
                            Ok(())
                        },
                    );
                    responder.send(Ok(())).unwrap();
                }
                _ => {
                    assert!(false, "got unexpected request: {:?}", req);
                }
            }
        })
    }

    fn serve_responsive_rcs(server_end: ServerEnd<RemoteControlMarker>) {
        serve_stream::<RemoteControlMarker, _, _>(
            server_end.into_stream(),
            move |req| async move {
                match req {
                    RemoteControlRequest::IdentifyHost { responder } => responder
                        .send(Ok(&IdentifyHostResponse {
                            addresses: Some(vec![]),
                            nodename: Some(NODENAME.to_string()),
                            ..Default::default()
                        }))
                        .unwrap(),
                    _ => panic!("Unexpected request: {:?}", req),
                }
            },
        );
    }
    fn serve_unresponsive_rcs(
        server_end: ServerEnd<RemoteControlMarker>,
        waiter: Shared<Receiver<()>>,
    ) {
        serve_stream::<RemoteControlMarker, _, _>(server_end.into_stream(), move |req| {
            let waiter = waiter.clone();
            async move {
                match req {
                    RemoteControlRequest::IdentifyHost { responder: _ } => {
                        waiter.await.unwrap();
                    }
                    _ => panic!("Unexpected request: {:?}", req),
                }
            }
        });
    }

    fn setup_responsive_daemon_server_with_fastboot_target() -> DaemonProxy {
        spawn_local_stream_handler(move |req| async move {
            match req {
                DaemonRequest::GetVersionInfo { responder } => {
                    responder.send(&daemon_version_info()).unwrap();
                }
                DaemonRequest::ConnectToProtocol { name: _, server_end, responder } => {
                    spawn_target_collection(
                        server_end,
                        |_| {
                            vec![TargetInfo {
                                nodename: Some(FASTBOOT_NODENAME.to_string()),
                                serial_number: Some(SERIAL_NUMBER.to_string()),
                                addresses: Some(vec![]),
                                age_ms: Some(0),
                                rcs_state: Some(RemoteControlState::Unknown),
                                target_state: Some(TargetState::Fastboot),
                                ..Default::default()
                            }]
                        },
                        |_query, target_handle| {
                            spawn_target_handler(target_handle, |req| match req {
                                TargetRequest::OpenRemoteControl { responder, remote_control } => {
                                    serve_responsive_rcs(remote_control);
                                    responder.send(Ok(())).unwrap();
                                }
                                r => panic!("unexpected request: {:?}", r),
                            });
                            Ok(())
                        },
                    );
                    responder.send(Ok(())).unwrap();
                }
                req => {
                    assert!(false, "got unexpected request: {:?}", req);
                }
            }
        })
    }

    fn setup_responsive_daemon_server_with_targets(
        has_nodename: bool,
        ssh_error: Option<&'static str>,
        waiter: Shared<Receiver<()>>,
    ) -> DaemonProxy {
        spawn_local_stream_handler(move |req| {
            let waiter = waiter.clone();
            async move {
                let nodename = if has_nodename { Some(NODENAME.to_string()) } else { None };
                match req {
                    DaemonRequest::GetVersionInfo { responder } => {
                        responder.send(&daemon_version_info()).unwrap();
                    }
                    DaemonRequest::ConnectToProtocol { name: _, server_end, responder } => {
                        let nodename = nodename.clone();
                        let waiter = waiter.clone();
                        spawn_target_collection(
                            server_end,
                            move |query| {
                                let query = query.string_matcher.as_deref().unwrap_or("");
                                if !query.is_empty()
                                    && query != NODENAME
                                    && query != UNRESPONSIVE_NODENAME
                                    && query != SSH_ERR_NODENAME
                                {
                                    vec![]
                                } else if query == NODENAME {
                                    vec![TargetInfo {
                                        nodename: nodename.clone(),
                                        addresses: Some(vec![]),
                                        age_ms: Some(0),
                                        rcs_state: Some(RemoteControlState::Unknown),
                                        target_state: Some(TargetState::Unknown),
                                        ..Default::default()
                                    }]
                                } else if ssh_error.is_some() {
                                    vec![TargetInfo {
                                        nodename: Some(SSH_ERR_NODENAME.to_string()),
                                        addresses: Some(vec![]),
                                        age_ms: Some(0),
                                        rcs_state: Some(RemoteControlState::Unknown),
                                        target_state: Some(TargetState::Unknown),
                                        ..Default::default()
                                    }]
                                } else {
                                    vec![
                                        TargetInfo {
                                            nodename: nodename.clone(),
                                            addresses: Some(vec![]),
                                            age_ms: Some(0),
                                            rcs_state: Some(RemoteControlState::Unknown),
                                            target_state: Some(TargetState::Unknown),
                                            ..Default::default()
                                        },
                                        TargetInfo {
                                            nodename: Some(UNRESPONSIVE_NODENAME.to_string()),
                                            addresses: Some(vec![]),
                                            age_ms: Some(0),
                                            rcs_state: Some(RemoteControlState::Unknown),
                                            target_state: Some(TargetState::Unknown),
                                            ..Default::default()
                                        },
                                    ]
                                }
                            },
                            move |query, target_handle| {
                                let waiter = waiter.clone();
                                spawn_target_handler(target_handle, move |req| match req {
                                    TargetRequest::OpenRemoteControl {
                                        responder,
                                        remote_control,
                                    } => {
                                        let target =
                                            query.string_matcher.as_deref().unwrap_or(NODENAME);
                                        if target == UNRESPONSIVE_NODENAME || !has_nodename {
                                            serve_unresponsive_rcs(remote_control, waiter.clone());
                                        } else if target == NODENAME || target == SSH_ERR_NODENAME {
                                            serve_responsive_rcs(remote_control);
                                        } else {
                                            panic!("got unexpected target string: '{}'", target);
                                        }
                                        if target == SSH_ERR_NODENAME {
                                            responder
                                                .send(Err(TargetConnectionError::UnknownError))
                                                .unwrap();
                                        } else {
                                            responder.send(Ok(())).unwrap();
                                        }
                                    }
                                    TargetRequest::GetSshLogs { responder } => {
                                        // This shouldn't even be requested if there is no ssh error
                                        responder.send(ssh_error.unwrap()).unwrap();
                                    }
                                    r => panic!("unexpected request: {:?}", r),
                                });
                                Ok(())
                            },
                        );
                        responder.send(Ok(())).unwrap();
                    }
                    _ => {
                        assert!(false, "got unexpected request: {:?}", req);
                    }
                }
            }
        })
    }

    fn setup_daemon_server_list_fails() -> DaemonProxy {
        spawn_local_stream_handler(move |req| async move {
            match req {
                DaemonRequest::GetVersionInfo { responder } => {
                    responder.send(&daemon_version_info()).unwrap();
                }
                DaemonRequest::ConnectToProtocol { name: _, server_end: _, responder } => {
                    // Do nothing with the server_end.
                    responder.send(Ok(())).unwrap();
                }
                _ => {
                    assert!(false, "got unexpected request: {:?}", req);
                }
            }
        })
    }

    fn setup_daemon_server_echo_hangs(waiter: Shared<Receiver<()>>) -> DaemonProxy {
        spawn_local_stream_handler(move |req| {
            let waiter = waiter.clone();
            async move {
                match req {
                    DaemonRequest::GetVersionInfo { responder: _ } => {
                        waiter.await.unwrap();
                    }
                    _ => {
                        assert!(false, "got unexpected request: {:?}", req);
                    }
                }
            }
        })
    }

    fn ffx_path() -> String {
        format!("{}", std::env::current_exe().unwrap().display())
    }

    fn frontend_version_info(use_default_api_level: bool) -> VersionInfo {
        VersionInfo {
            commit_hash: None,
            commit_timestamp: None,
            build_version: Some(FRONTEND_VERSION.to_string()),
            abi_revision: Some(FAKE_ABI_REVISION),
            api_level: if use_default_api_level {
                Some(FAKE_API_LEVEL)
            } else {
                Some(ANOTHER_FAKE_API_LEVEL)
            },
            ..Default::default()
        }
    }

    fn daemon_version_info() -> fidl_fuchsia_developer_ffx::VersionInfo {
        fidl_fuchsia_developer_ffx::VersionInfo {
            commit_hash: None,
            commit_timestamp: None,
            build_version: Some(DAEMON_VERSION.to_string()),
            abi_revision: Some(FAKE_ABI_REVISION),
            api_level: Some(FAKE_API_LEVEL),
            exec_path: Some(ffx_path()),
            ..Default::default()
        }
    }

    fn record_params_no_record() -> DoctorRecorderParameters {
        DoctorRecorderParameters {
            record: false,
            user_config_enabled: false,
            log_root: None,
            output_dir: None,
            recorder: Arc::new(Mutex::new(DisabledRecorder::new())),
        }
    }

    fn record_params_with_temp(
        root: PathBuf,
    ) -> (Arc<Mutex<FakeRecorder>>, DoctorRecorderParameters) {
        let mut fe_log = root.clone();
        fe_log.push("ffx.log");
        let mut daemon_log = root.clone();
        daemon_log.push("ffx.daemon.log");
        fs::write(&fe_log, "ffx.log contents").expect("writing test ffx.log");
        fs::write(&daemon_log, "ffx.daemon.log contents").expect("writing test ffx.daemon.log");
        let recorder =
            Arc::new(Mutex::new(FakeRecorder::new(vec![fe_log, daemon_log], root.clone())));
        (
            recorder.clone(),
            DoctorRecorderParameters {
                record: true,
                user_config_enabled: false,
                log_root: Some(root.clone()),
                output_dir: Some(root.clone()),
                recorder: recorder.clone(),
            },
        )
    }

    fn setup_emu_dir(isolate_root: &Path) -> Result<PathBuf> {
        let emu_dir = isolate_root.join("emu_data");
        fs::create_dir_all(&emu_dir)?;
        Ok(emu_dir)
    }

    async fn setup_ssh_keys(isolate_root: &Path) -> Result<(PathBuf, PathBuf)> {
        let pub_key = isolate_root.join("test_authorized_keys");
        let priv_key = isolate_root.join("test_ed25519_key");
        let keys = SshKeyFiles { authorized_keys: pub_key.clone(), private_key: priv_key.clone() };
        keys.create_keys_if_needed(false)?;
        Ok((pub_key, priv_key))
    }

    async fn setup_driver_socket_file(isolate_root: &Path) -> Result<PathBuf> {
        let socket_file_dir = isolate_root.join("test_usb_driver_socket");
        fs::create_dir_all(&socket_file_dir)?;
        let socket_file = socket_file_dir.join("socket");
        std::fs::File::create(&socket_file)?;
        Ok(socket_file)
    }

    async fn setup_test_env() -> Result<ffx_config::TestEnv> {
        let mut builder = ffx_config::test_env();
        let isolate_root = builder.isolate_root();

        let (pub_key, priv_key) = setup_ssh_keys(&isolate_root).await?;
        let emu_dir = setup_emu_dir(&isolate_root)?;
        let socket_file = setup_driver_socket_file(&isolate_root).await?;

        let test_env = builder
            .user_config("ssh.pub", json!([&pub_key]))
            .user_config("ssh.priv", json!([&priv_key]))
            .user_config(ffx_config::keys::EMU_INSTANCE_ROOT_DIR, json!(&emu_dir))
            .user_config(usb_driver_api::CONFIG_USB_SOCKET_PATH, json!(socket_file))
            .user_config(ffx_config::keys::USB_ENABLED, json!(true))
            .build()
            .unwrap();
        Ok(test_env)
    }

    fn default_mock_driver_finder() -> MockUsbDriverFinder {
        let mut mock = MockUsbDriverFinder::new();
        mock.expect_find().returning(|| {
            Ok(vec![UsbDriverStatus { pid: 1, socket_path: "/tmp/fake/socket/path".to_string() }])
        });
        mock
    }

    #[fuchsia::test]
    async fn test_single_try_no_daemon_running_no_targets_with_default_target() {
        let test_env = setup_test_env().await.unwrap();

        let fake = FakeDaemonManager::new(
            vec![false],
            vec![Ok(false)],
            vec![Ok(())],
            vec![Ok(setup_responsive_daemon_server())],
            vec![],
        );

        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        let mock_driver_finder = default_mock_driver_finder();
        doctor(
            &mut handler,
            &mut ledger.root_guard(),
            false,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(Some(NODENAME.to_string())),
            &test_env.context,
            record_params_no_record(),
            mock_driver_finder,
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
                   \n[✓] FFX doctor\
                   \n    [✓] Frontend version: {FRONTEND_VERSION}\
                   \n    [✓] abi-revision: {ABI_REVISION_STR}\
                   \n    [✓] api-level: {FAKE_API_LEVEL}\
                   \n    [i] Path to ffx: {ffx_path}\
                   \n[✓] FFX Environment Context\
                   \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
                   \n    [✓] Environment File Location: {env_file}\
                   \n    [✓] No build directory discovered in the environment.\
                   \n    [✓] Config Lock Files\
                   \n        [✓] {user_file} locked by {user_file}.lock\
                   \n        [✓] {global_file} locked by {global_file}.lock\
                   \n    [✓] The public & private Fuchsia keys are consistent\
                   \n[✓] FFX Emulator Instances\
                   \n    [i] No Emulator instances\
                   \n[✗] Checking daemon\
                   \n    [✗] No running daemons found. Run `ffx doctor --restart-daemon`\
                   \n[!] FFX USB Driver\
                   \n    [!] ffx-usb-driver is running.\
                   \n        [✓] PID: 1\
                   \n        [✓] Socket: /tmp/fake/socket/path\
                   \n        [!] ffx-usb-driver is listening on a different socket than what is configured: /tmp/fake/socket/path. Expected: {driver_socket_path}\
                   \n[✗] Google Network Checks\
                   \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
                   \n[✗] Doctor found issues in one or more categories.\n",
                ffx_path = ffx_path(),
                isolated_root = test_env.isolate_root.path().display(),
                env_file = test_env.env_file.path().display(),
                user_file = test_env.user_file.path().display(),
                global_file = test_env.global_file.path().display(),
                driver_socket_path =
                    test_env.isolate_root.path().join("test_usb_driver_socket/socket").display()
            )
        );
    }

    #[fuchsia::test]
    async fn test_usb_driver_not_running() {
        let mut builder = ffx_config::test_env();
        let isolate_root = builder.isolate_root();

        let (pub_key, priv_key) =
            setup_ssh_keys(&isolate_root).await.expect("setting up ssh test keys");
        let emu_dir = setup_emu_dir(&isolate_root).expect("setting up emulator data");

        let test_env = builder
            .user_config("ssh.pub", json!([&pub_key]))
            .user_config("ssh.priv", json!([&priv_key]))
            .user_config(ffx_config::keys::EMU_INSTANCE_ROOT_DIR, json!(&emu_dir))
            .user_config(ffx_config::keys::USB_ENABLED, json!(true))
            .build()
            .unwrap();

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server())],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        let mut mock_driver_finder = MockUsbDriverFinder::new();
        mock_driver_finder
            .expect_find()
            .times(1)
            .returning(|| Err(FindUsbDriverError::DriverIsNotRunning));

        doctor(
            &mut handler,
            &mut ledger.root_guard(),
            false,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            record_params_no_record(),
            mock_driver_finder,
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
            \n[✓] FFX doctor\
            \n    [✓] Frontend version: {FRONTEND_VERSION}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [i] Path to ffx: {ffx_path}\
            \n[✓] FFX Environment Context\
            \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
            \n    [✓] Environment File Location: {env_file}\
            \n    [✓] No build directory discovered in the environment.\
            \n    [✓] Config Lock Files\
            \n        [✓] {user_file} locked by {user_file}.lock\
            \n        [✓] {global_file} locked by {global_file}.lock\
            \n    [✓] The public & private Fuchsia keys are consistent\
            \n[✓] FFX Emulator Instances\
            \n    [i] No Emulator instances\
            \n[✓] Checking daemon\
            \n    [✓] Daemon found: [1]\
            \n    [✓] Connecting to daemon\
            \n    [✓] Daemon version: {DAEMON_VERSION}\
            \n    [✓] path: {ffx_path}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [✓] Default target: (none)\
            \n[!] FFX USB Driver\
            \n    [!] The ffx-usb-driver is not running. It should be started automatically when \
            needed. If this error persists and there are ongoing issues communicating with the \
            target, this may be a bug.\
            \n[✗] Google Network Checks\
            \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
            \n[✗] Searching for targets\
            \n    [✗] No targets found!\
            \n[✗] Doctor found issues in one or more categories.\n",
                ffx_path = ffx_path(),
                isolated_root = test_env.isolate_root.path().display(),
                env_file = test_env.env_file.path().display(),
                user_file = test_env.user_file.path().display(),
                global_file = test_env.global_file.path().display(),
            )
        );
    }

    #[fuchsia::test]
    async fn test_usb_driver_disabled() {
        let mut builder = ffx_config::test_env();
        let isolate_root = builder.isolate_root();

        let (pub_key, priv_key) =
            setup_ssh_keys(&isolate_root).await.expect("setting up ssh test keys");
        let emu_dir = setup_emu_dir(&isolate_root).expect("setting up emulator data");

        let test_env = builder
            .user_config("ssh.pub", json!([&pub_key]))
            .user_config("ssh.priv", json!([&priv_key]))
            .user_config(ffx_config::keys::EMU_INSTANCE_ROOT_DIR, json!(&emu_dir))
            .user_config(ffx_config::keys::USB_ENABLED, json!(false))
            .build()
            .unwrap();

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server())],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        let mut mock_driver_finder = MockUsbDriverFinder::new();
        mock_driver_finder.expect_find().times(0);

        doctor(
            &mut handler,
            &mut ledger.root_guard(),
            false,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            record_params_no_record(),
            mock_driver_finder,
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        let output = ledger.writer.get_data();
        assert!(!output.contains("FFX USB Driver"), "Output contains FFX USB Driver: {}", output);
    }

    #[fuchsia::test]
    async fn test_single_try_daemon_running_no_targets() {
        let test_env = setup_test_env().await.unwrap();

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server())],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        let mock_driver_finder = default_mock_driver_finder();
        doctor(
            &mut handler,
            &mut ledger.root_guard(),
            false,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            record_params_no_record(),
            mock_driver_finder,
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
                   \n[✓] FFX doctor\
                   \n    [✓] Frontend version: {FRONTEND_VERSION}\
                   \n    [✓] abi-revision: {ABI_REVISION_STR}\
                   \n    [✓] api-level: {FAKE_API_LEVEL}\
                   \n    [i] Path to ffx: {ffx_path}\
                   \n[✓] FFX Environment Context\
                   \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
                   \n    [✓] Environment File Location: {env_file}\
                   \n    [✓] No build directory discovered in the environment.\
                   \n    [✓] Config Lock Files\
                   \n        [✓] {user_file} locked by {user_file}.lock\
                   \n        [✓] {global_file} locked by {global_file}.lock\
                   \n    [✓] The public & private Fuchsia keys are consistent\
                   \n[✓] FFX Emulator Instances\
                   \n    [i] No Emulator instances\
                   \n[✓] Checking daemon\
                   \n    [✓] Daemon found: [1]\
                   \n    [✓] Connecting to daemon\
                   \n    [✓] Daemon version: {DAEMON_VERSION}\
                   \n    [✓] path: {ffx_path}\
                   \n    [✓] abi-revision: {ABI_REVISION_STR}\
                   \n    [✓] api-level: {FAKE_API_LEVEL}\
                   \n    [✓] Default target: (none)\
                   \n[!] FFX USB Driver\
                   \n    [!] ffx-usb-driver is running.\
                   \n        [✓] PID: 1\
                   \n        [✓] Socket: /tmp/fake/socket/path\
                   \n        [!] ffx-usb-driver is listening on a different socket than what is configured: /tmp/fake/socket/path. Expected: {driver_socket_path}\
                   \n[✗] Google Network Checks\
                   \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
                   \n[✗] Searching for targets\
                   \n    [✗] No targets found!\
                   \n[✗] Doctor found issues in one or more categories.\n",
                ffx_path = ffx_path(),
                isolated_root = test_env.isolate_root.path().display(),
                env_file = test_env.env_file.path().display(),
                user_file = test_env.user_file.path().display(),
                global_file = test_env.global_file.path().display(),
                driver_socket_path =
                    test_env.isolate_root.path().join("test_usb_driver_socket/socket").display()
            )
        );
    }

    #[fuchsia::test]
    async fn test_single_try_daemon_running_connection_error() {
        let test_env = setup_test_env().await.unwrap();

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Err(doctor_utils::DoctorUtilsError::Daemon(Box::new(
                ffx_daemon::DaemonError::Circuit("Some error message".to_string()),
            )))],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new_with_error_reason());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        let mock_driver_finder = default_mock_driver_finder();
        doctor(
            &mut handler,
            &mut ledger.root_guard(),
            false,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(Some("".to_string())),
            &test_env.context,
            record_params_no_record(),
            mock_driver_finder,
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
                   \n[✓] FFX doctor\
                   \n    [✓] Frontend version: {FRONTEND_VERSION}\
                   \n    [✓] abi-revision: {ABI_REVISION_STR}\
                   \n    [✓] api-level: {FAKE_API_LEVEL}\
                   \n    [i] Path to ffx: {ffx_path}\
                   \n[✓] FFX Environment Context\
                   \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
                   \n    [✓] Environment File Location: {env_file}\
                   \n    [✓] No build directory discovered in the environment.\
                   \n    [✓] Config Lock Files\
                   \n        [✓] {user_file} locked by {user_file}.lock\
                   \n        [✓] {global_file} locked by {global_file}.lock\
                   \n    [✓] The public & private Fuchsia keys are consistent\
                   \n[✓] FFX Emulator Instances\
                   \n    [i] No Emulator instances\
                   \n[✗] Checking daemon\
                   \n    [✓] Daemon found: [1]\
                   \n    [✗] Error connecting to daemon: Daemon core error: Circuit error: Some error message. Run `ffx doctor --restart-daemon`\
                   \n[!] FFX USB Driver\
                   \n    [!] ffx-usb-driver is running.\
                   \n        [✓] PID: 1\
                   \n        [✓] Socket: /tmp/fake/socket/path\
                   \n        [!] ffx-usb-driver is listening on a different socket than what is configured: /tmp/fake/socket/path. Expected: {driver_socket_path}\
                   \n[✗] Google Network Checks\
                   \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
                   \n[✗] Doctor found issues in one or more categories.\n",
                ffx_path = ffx_path(),
                isolated_root = test_env.isolate_root.path().display(),
                env_file = test_env.env_file.path().display(),
                user_file = test_env.user_file.path().display(),
                global_file = test_env.global_file.path().display(),
                driver_socket_path =
                    test_env.isolate_root.path().join("test_usb_driver_socket/socket").display()
            )
        );
    }

    #[fuchsia::test]
    async fn test_single_try_daemon_running_no_targets_default_target_empty() {
        let test_env = setup_test_env().await.unwrap();

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server())],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        let mock_driver_finder = default_mock_driver_finder();
        doctor(
            &mut handler,
            &mut ledger.root_guard(),
            false,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(Some("".to_string())),
            &test_env.context,
            record_params_no_record(),
            mock_driver_finder,
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
                   \n[✓] FFX doctor\
                   \n    [✓] Frontend version: {FRONTEND_VERSION}\
                   \n    [✓] abi-revision: {ABI_REVISION_STR}\
                   \n    [✓] api-level: {FAKE_API_LEVEL}\
                   \n    [i] Path to ffx: {ffx_path}\
                   \n[✓] FFX Environment Context\
                   \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
                   \n    [✓] Environment File Location: {env_file}\
                   \n    [✓] No build directory discovered in the environment.\
                   \n    [✓] Config Lock Files\
                   \n        [✓] {user_file} locked by {user_file}.lock\
                   \n        [✓] {global_file} locked by {global_file}.lock\
                   \n    [✓] The public & private Fuchsia keys are consistent\
                   \n[✓] FFX Emulator Instances\
                   \n    [i] No Emulator instances\
                   \n[✓] Checking daemon\
                   \n    [✓] Daemon found: [1]\
                   \n    [✓] Connecting to daemon\
                   \n    [✓] Daemon version: {DAEMON_VERSION}\
                   \n    [✓] path: {ffx_path}\
                   \n    [✓] abi-revision: {ABI_REVISION_STR}\
                   \n    [✓] api-level: {FAKE_API_LEVEL}\
                   \n    [✓] Default target: (none)\
                   \n[!] FFX USB Driver\
                   \n    [!] ffx-usb-driver is running.\
                   \n        [✓] PID: 1\
                   \n        [✓] Socket: /tmp/fake/socket/path\
                   \n        [!] ffx-usb-driver is listening on a different socket than what is configured: /tmp/fake/socket/path. Expected: {driver_socket_path}\
                   \n[✗] Google Network Checks\
                   \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
                   \n[✗] Searching for targets\
                   \n    [✗] No targets found!\
                   \n[✗] Doctor found issues in one or more categories.\n",
                ffx_path = ffx_path(),
                isolated_root = test_env.isolate_root.path().display(),
                env_file = test_env.env_file.path().display(),
                user_file = test_env.user_file.path().display(),
                global_file = test_env.global_file.path().display(),
                driver_socket_path =
                    test_env.isolate_root.path().join("test_usb_driver_socket/socket").display()
            )
        );
    }

    #[fuchsia::test]
    async fn test_two_tries_daemon_running_list_fails() {
        let test_env = setup_test_env().await.unwrap();

        let fake = FakeDaemonManager::new(
            vec![true, false],
            vec![Ok(true), Ok(false)],
            vec![Ok(())],
            vec![Ok(setup_daemon_server_list_fails()), Ok(setup_daemon_server_list_fails())],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        let mock_driver_finder = default_mock_driver_finder();
        doctor(
            &mut handler,
            &mut ledger.root_guard(),
            false,
            &fake,
            "",
            2,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            record_params_no_record(),
            mock_driver_finder,
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
            \n[✓] FFX doctor\
            \n    [✓] Frontend version: {FRONTEND_VERSION}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [i] Path to ffx: {ffx_path}\
            \n[✓] FFX Environment Context\
            \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
            \n    [✓] Environment File Location: {env_file}\
            \n    [✓] No build directory discovered in the environment.\
            \n    [✓] Config Lock Files\
            \n        [✓] {user_file} locked by {user_file}.lock\
            \n        [✓] {global_file} locked by {global_file}.lock\
            \n    [✓] The public & private Fuchsia keys are consistent\
            \n[✓] FFX Emulator Instances\
            \n    [i] No Emulator instances\
            \n[✓] Checking daemon\
            \n    [✓] Daemon found: [1]\
            \n    [✓] Connecting to daemon\
            \n    [✓] Daemon version: {DAEMON_VERSION}\
            \n    [✓] path: {ffx_path}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [✓] Default target: (none)\
            \n[!] FFX USB Driver\
            \n    [!] ffx-usb-driver is running.\
            \n        [✓] PID: 1\
            \n        [✓] Socket: /tmp/fake/socket/path\
            \n        [!] ffx-usb-driver is listening on a different socket than what is configured: /tmp/fake/socket/path. Expected: {driver_socket_path}\
            \n[✗] Google Network Checks\
            \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
            \n[✗] Searching for targets\
            \n    [✗] No targets found!\
            \n[✗] Doctor found issues in one or more categories.\n",
                ffx_path = ffx_path(),
                isolated_root = test_env.isolate_root.path().display(),
                env_file = test_env.env_file.path().display(),
                user_file = test_env.user_file.path().display(),
                global_file = test_env.global_file.path().display(),
                driver_socket_path =
                    test_env.isolate_root.path().join("test_usb_driver_socket/socket").display()
            )
        );
    }

    #[fuchsia::test]
    async fn test_two_tries_no_daemon_running_echo_timeout() {
        let (tx, rx) = oneshot::channel::<()>();

        let fake = FakeDaemonManager::new(
            vec![false, true],
            vec![Ok(false), Ok(true)],
            vec![Ok(()), Ok(())],
            vec![
                Ok(setup_daemon_server_echo_hangs(rx.shared())),
                Ok(setup_responsive_daemon_server()),
            ],
            vec![Ok(vec![]), Ok(vec![]), Ok(vec![1]), Ok(vec![2]), Ok(vec![]), Ok(vec![3])],
        );

        // restart daemon
        {
            let ledger_view = Box::new(FakeLedgerView::new());
            let mut ledger = DoctorLedger::<MockWriter>::new(
                MockWriter::new(),
                ledger_view,
                LedgerViewMode::Verbose,
            );

            doctor_daemon_restart(&fake, DEFAULT_RETRY_DELAY, &mut ledger.root_guard()).await;

            assert_eq!(
                ledger.writer.get_data(),
                "\
                    \n[✓] Killing Daemon\
                    \n    [✓] No running daemons found.\
                    \n[✗] Starting Daemon\
                    \n    [✓] Daemon spawned\
                    \n    [✓] Daemon PID: [1]\
                    \n    [✓] Connected to daemon\
                    \n    [✗] Timeout while getting daemon version\
                    \n"
            );
        }

        // restart daemon
        {
            let ledger_view = Box::new(FakeLedgerView::new());
            let mut ledger = DoctorLedger::<MockWriter>::new(
                MockWriter::new(),
                ledger_view,
                LedgerViewMode::Verbose,
            );

            doctor_daemon_restart(&fake, DEFAULT_RETRY_DELAY, &mut ledger.root_guard()).await;

            assert_eq!(
                ledger.writer.get_data(),
                format!(
                    "\
                    \n[✓] Killing Daemon\
                    \n    [✓] Killing running daemons.\
                    \n    [✓] Killed daemon PID: [2]\
                    \n[✓] Starting Daemon\
                    \n    [✓] Daemon spawned\
                    \n    [✓] Daemon PID: [3]\
                    \n    [✓] Connected to daemon\
                    \n    [✓] Daemon version: {DAEMON_VERSION}\
                    \n    [✓] abi-revision: {ABI_REVISION_STR}\
                    \n    [✓] api-level: {FAKE_API_LEVEL}\
                    \n",
                )
            );
        }

        tx.send(()).unwrap();
    }

    struct RcsTestArgs {
        ledger_mode: LedgerViewMode,
        ssh_error: Option<&'static str>,
        with_reason: bool,
    }

    impl Default for RcsTestArgs {
        fn default() -> Self {
            RcsTestArgs { ledger_mode: LedgerViewMode::Normal, ssh_error: None, with_reason: false }
        }
    }

    impl RcsTestArgs {
        fn verbose(mut self) -> Self {
            self.ledger_mode = LedgerViewMode::Verbose;
            self
        }

        fn with_ssh_error(mut self, e: &'static str) -> Self {
            self.ssh_error = Some(e);
            self
        }

        fn with_reason(mut self) -> Self {
            self.with_reason = true;
            self
        }
    }

    async fn test_finds_target_connects_to_rcs_setup(
        test_env: &TestEnv,
        modes: RcsTestArgs,
    ) -> DoctorLedger<MockWriter> {
        let (tx, rx) = oneshot::channel::<()>();

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server_with_targets(
                true,
                modes.ssh_error,
                rx.shared(),
            ))],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(if modes.with_reason {
            FakeLedgerView::new_with_error_reason()
        } else {
            FakeLedgerView::new()
        });
        let mut ledger =
            DoctorLedger::<MockWriter>::new(MockWriter::new(), ledger_view, modes.ledger_mode);

        let mock_driver_finder = default_mock_driver_finder();
        doctor(
            &mut handler,
            &mut ledger.root_guard(),
            false,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            record_params_no_record(),
            mock_driver_finder,
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();
        tx.send(()).unwrap();

        return ledger;
    }

    #[fuchsia::test]
    async fn test_finds_target_connects_to_rcs_with_ssh_error_verbose() {
        let test_env = setup_test_env().await.unwrap();
        let ledger = test_finds_target_connects_to_rcs_setup(
            &test_env,
            RcsTestArgs::default().verbose().with_ssh_error("some ssh error"),
        )
        .await;
        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
            \n[✓] FFX doctor\
            \n    [✓] Frontend version: {FRONTEND_VERSION}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [i] Path to ffx: {ffx_path}\
            \n[✓] FFX Environment Context\
            \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
            \n    [✓] Environment File Location: {env_file}\
            \n    [✓] No build directory discovered in the environment.\
            \n    [✓] Config Lock Files\
            \n        [✓] {user_file} locked by {user_file}.lock\
            \n        [✓] {global_file} locked by {global_file}.lock\
            \n    [✓] The public & private Fuchsia keys are consistent\
            \n[✓] FFX Emulator Instances\
            \n    [i] No Emulator instances\
            \n[✓] Checking daemon\
            \n    [✓] Daemon found: [1]\
            \n    [✓] Connecting to daemon\
            \n    [✓] Daemon version: {DAEMON_VERSION}\
            \n    [✓] path: {ffx_path}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [✓] Default target: (none)\
            \n[!] FFX USB Driver\
            \n    [!] ffx-usb-driver is running.\
            \n        [✓] PID: 1\
            \n        [✓] Socket: /tmp/fake/socket/path\
            \n        [!] ffx-usb-driver is listening on a different socket than what is configured: /tmp/fake/socket/path. Expected: {driver_socket_path}\
            \n[✗] Google Network Checks\
            \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
            \n[✓] Searching for targets\
            \n    [✓] 1 targets found\
            \n[✗] Target: {SSH_ERR_NODENAME}\
            \n    [!] Compatibility state: absent\
            \n        [!] Compatibility information is not available\
            \n    [✓] Opened target handle\
            \n    [✓] Connecting to RCS\
            \n    [✗] Error while connecting to RCS: <reason omitted>\
            \n[✗] Doctor found issues in one or more categories.\n",
                ffx_path = ffx_path(),
                isolated_root = test_env.isolate_root.path().display(),
                env_file = test_env.env_file.path().display(),
                user_file = test_env.user_file.path().display(),
                global_file = test_env.global_file.path().display(),
                driver_socket_path =
                    test_env.isolate_root.path().join("test_usb_driver_socket/socket").display()
            )
        );
    }

    #[fuchsia::test]
    async fn test_ssh_connection_refused_recommends_tunnel() {
        let test_env = setup_test_env().await.unwrap();
        let ledger = test_finds_target_connects_to_rcs_setup(
            &test_env,
            RcsTestArgs::default().with_ssh_error("Connection refused").with_reason(),
        )
        .await;
        let output = ledger.writer.get_data();
        assert!(output.contains(
            "[i] SSH connection was refused. You may need to (re-)establish a tunnel connection.\n"
        ));
    }

    #[fuchsia::test]
    async fn test_ssh_permission_denied_recommends_repave() {
        let test_env = setup_test_env().await.unwrap();
        let ledger = test_finds_target_connects_to_rcs_setup(
            &test_env,
            RcsTestArgs::default()
                .with_ssh_error("Permission denied (publickey,keyboard-interactive)")
                .with_reason(),
        )
        .await;
        let output = ledger.writer.get_data();
        assert!(output.contains(
            "[i] SSH connection could not authenticate. You may need to re-provision (pave or flash) your target to ensure SSH keys are appropriately setup.\n"
        ));
    }

    #[fuchsia::test]
    async fn test_finds_target_connects_to_rcs_verbose() {
        let test_env = setup_test_env().await.unwrap();
        let ledger =
            test_finds_target_connects_to_rcs_setup(&test_env, RcsTestArgs::default().verbose())
                .await;
        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
            \n[✓] FFX doctor\
            \n    [✓] Frontend version: {FRONTEND_VERSION}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [i] Path to ffx: {ffx_path}\
            \n[✓] FFX Environment Context\
            \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
            \n    [✓] Environment File Location: {env_file}\
            \n    [✓] No build directory discovered in the environment.\
            \n    [✓] Config Lock Files\
            \n        [✓] {user_file} locked by {user_file}.lock\
            \n        [✓] {global_file} locked by {global_file}.lock\
            \n    [✓] The public & private Fuchsia keys are consistent\
            \n[✓] FFX Emulator Instances\
            \n    [i] No Emulator instances\
            \n[✓] Checking daemon\
            \n    [✓] Daemon found: [1]\
            \n    [✓] Connecting to daemon\
            \n    [✓] Daemon version: {DAEMON_VERSION}\
            \n    [✓] path: {ffx_path}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [✓] Default target: (none)\
            \n[!] FFX USB Driver\
            \n    [!] ffx-usb-driver is running.\
            \n        [✓] PID: 1\
            \n        [✓] Socket: /tmp/fake/socket/path\
            \n        [!] ffx-usb-driver is listening on a different socket than what is configured: /tmp/fake/socket/path. Expected: {driver_socket_path}\
            \n[✗] Google Network Checks\
            \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
            \n[✓] Searching for targets\
            \n    [✓] 2 targets found\
            \n[✓] Target: {NODENAME}\
            \n    [!] Compatibility state: absent\
            \n        [!] Compatibility information is not available\
            \n    [✓] Opened target handle\
            \n    [✓] Connecting to RCS\
            \n    [✓] Communicating with RCS\
            \n[✗] Target: {UNRESPONSIVE_NODENAME}\
            \n    [!] Compatibility state: absent\
            \n        [!] Compatibility information is not available\
            \n    [✓] Opened target handle\
            \n    [✓] Connecting to RCS\
            \n    [✗] Timeout while communicating with RCS\
            \n[✗] Doctor found issues in one or more categories.\n",
                ffx_path = ffx_path(),
                isolated_root = test_env.isolate_root.path().display(),
                env_file = test_env.env_file.path().display(),
                user_file = test_env.user_file.path().display(),
                global_file = test_env.global_file.path().display(),
                driver_socket_path =
                    test_env.isolate_root.path().join("test_usb_driver_socket/socket").display()
            )
        );
    }

    #[fuchsia::test]
    async fn test_finds_target_connects_to_rcs_normal() {
        let test_env = setup_test_env().await.unwrap();
        let ledger =
            test_finds_target_connects_to_rcs_setup(&test_env, RcsTestArgs::default()).await;
        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
                \n[✓] FFX doctor\
                \n    [i] Path to ffx: {ffx_path}\
                \n[✓] FFX Environment Context\
                \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
                \n    [✓] Config Lock Files\
                \n        [✓] {user_file} locked by {user_file}.lock\
                \n        [✓] {global_file} locked by {global_file}.lock\
                \n    [✓] The public & private Fuchsia keys are consistent\
                \n[✓] FFX Emulator Instances\
                \n    [i] No Emulator instances\
                \n[✓] Checking daemon\
                \n    [✓] Daemon found: [1]\
                \n    [✓] Connecting to daemon\
                \n[!] FFX USB Driver\
                \n    [!] ffx-usb-driver is running.\
                \n        [!] ffx-usb-driver is listening on a different socket than what is configured: /tmp/fake/socket/path. Expected: {driver_socket_path}\
                \n[✗] Google Network Checks\
                \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
                \n[✓] Searching for targets\
                \n    [✓] 2 targets found\
                \n[✓] Target: {NODENAME}\
                \n[✗] Target: {UNRESPONSIVE_NODENAME}\
                \n[✗] Doctor found issues in one or more categories; \
                run 'ffx doctor -v' for more details.\n",
                ffx_path = ffx_path(),
                isolated_root = test_env.isolate_root.path().display(),
                user_file = test_env.user_file.path().display(),
                global_file = test_env.global_file.path().display(),
                driver_socket_path =
                    test_env.isolate_root.path().join("test_usb_driver_socket/socket").display()
            )
        );
    }

    #[fuchsia::test]
    async fn test_finds_target_with_filter() {
        let test_env = setup_test_env().await.unwrap();

        let (tx, rx) = oneshot::channel::<()>();

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server_with_targets(true, None, rx.shared()))],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        let mock_driver_finder = default_mock_driver_finder();
        doctor(
            &mut handler,
            &mut ledger.root_guard(),
            false,
            &fake,
            &NODENAME,
            2,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            record_params_no_record(),
            mock_driver_finder,
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();
        tx.send(()).unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
            \n[✓] FFX doctor\
            \n    [✓] Frontend version: {FRONTEND_VERSION}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [i] Path to ffx: {ffx_path}\
            \n[✓] FFX Environment Context\
            \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
            \n    [✓] Environment File Location: {env_file}\
            \n    [✓] No build directory discovered in the environment.\
            \n    [✓] Config Lock Files\
            \n        [✓] {user_file} locked by {user_file}.lock\
            \n        [✓] {global_file} locked by {global_file}.lock\
            \n    [✓] The public & private Fuchsia keys are consistent\
            \n[✓] FFX Emulator Instances\
            \n    [i] No Emulator instances\
            \n[✓] Checking daemon\
            \n    [✓] Daemon found: [1]\
            \n    [✓] Connecting to daemon\
            \n    [✓] Daemon version: {DAEMON_VERSION}\
            \n    [✓] path: {ffx_path}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [✓] Default target: (none)\
            \n[!] FFX USB Driver\
            \n    [!] ffx-usb-driver is running.\
            \n        [✓] PID: 1\
            \n        [✓] Socket: /tmp/fake/socket/path\
            \n        [!] ffx-usb-driver is listening on a different socket than what is configured: /tmp/fake/socket/path. Expected: {driver_socket_path}\
            \n[✗] Google Network Checks\
            \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
            \n[✓] Searching for targets\
            \n    [✓] 1 targets found\
            \n[✓] Target: {NODENAME}\
            \n    [!] Compatibility state: absent\
            \n        [!] Compatibility information is not available\
            \n    [✓] Opened target handle\
            \n    [✓] Connecting to RCS\
            \n    [✓] Communicating with RCS\
            \n[✓] No issues found\n",
                ffx_path = ffx_path(),
                isolated_root = test_env.isolate_root.path().display(),
                env_file = test_env.env_file.path().display(),
                user_file = test_env.user_file.path().display(),
                global_file = test_env.global_file.path().display(),
                driver_socket_path =
                    test_env.isolate_root.path().join("test_usb_driver_socket/socket").display()
            )
        );
    }

    #[fuchsia::test]
    async fn test_invalid_filter_finds_no_targets() {
        let test_env = setup_test_env().await.unwrap();

        let (tx, rx) = oneshot::channel::<()>();

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server_with_targets(true, None, rx.shared()))],
            vec![Ok(vec![1])],
        );

        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        let mock_driver_finder = default_mock_driver_finder();
        doctor(
            &mut handler,
            &mut ledger.root_guard(),
            false,
            &fake,
            &NON_EXISTENT_NODENAME,
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            record_params_no_record(),
            mock_driver_finder,
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();
        tx.send(()).unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
            \n[✓] FFX doctor\
            \n    [✓] Frontend version: {FRONTEND_VERSION}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [i] Path to ffx: {ffx_path}\
            \n[✓] FFX Environment Context\
            \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
            \n    [✓] Environment File Location: {env_file}\
            \n    [✓] No build directory discovered in the environment.\
            \n    [✓] Config Lock Files\
            \n        [✓] {user_file} locked by {user_file}.lock\
            \n        [✓] {global_file} locked by {global_file}.lock\
            \n    [✓] The public & private Fuchsia keys are consistent\
            \n[✓] FFX Emulator Instances\
            \n    [i] No Emulator instances\
            \n[✓] Checking daemon\
            \n    [✓] Daemon found: [1]\
            \n    [✓] Connecting to daemon\
            \n    [✓] Daemon version: {DAEMON_VERSION}\
            \n    [✓] path: {ffx_path}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [✓] Default target: (none)\
            \n[!] FFX USB Driver\
            \n    [!] ffx-usb-driver is running.\
            \n        [✓] PID: 1\
            \n        [✓] Socket: /tmp/fake/socket/path\
            \n        [!] ffx-usb-driver is listening on a different socket than what is configured: /tmp/fake/socket/path. Expected: {driver_socket_path}\
            \n[✗] Google Network Checks\
            \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
            \n[✗] Searching for targets\
            \n    [✗] No targets found!\
            \n[✗] Doctor found issues in one or more categories.\n",
                ffx_path = ffx_path(),
                isolated_root = test_env.isolate_root.path().display(),
                env_file = test_env.env_file.path().display(),
                user_file = test_env.user_file.path().display(),
                global_file = test_env.global_file.path().display(),
                driver_socket_path =
                    test_env.isolate_root.path().join("test_usb_driver_socket/socket").display()
            )
        );
    }

    #[fuchsia::test]
    async fn test_single_try_daemon_running_restart_daemon() {
        let fake = FakeDaemonManager::new(
            vec![false],
            vec![Ok(true), Ok(false)],
            vec![Ok(())],
            vec![Ok(setup_responsive_daemon_server())],
            vec![Ok(vec![1, 2, 3]), Ok(vec![]), Ok(vec![4])],
        );
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        doctor_daemon_restart(&fake, DEFAULT_RETRY_DELAY, &mut ledger.root_guard()).await;

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
            \n[✓] Killing Daemon\
            \n    [✓] Killing zombie daemons.\
            \n    [✓] Killed daemon PID: [1, 2, 3]\
            \n[✓] Starting Daemon\
            \n    [✓] Daemon spawned\
            \n    [✓] Daemon PID: [4]\
            \n    [✓] Connected to daemon\
            \n    [✓] Daemon version: {DAEMON_VERSION}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n"
            )
        );
    }

    #[fuchsia::test]
    async fn test_single_try_daemon_running_restart_daemon_pid_error() {
        let fake = FakeDaemonManager::new(
            vec![false],
            vec![Ok(true), Ok(false)],
            vec![Ok(())],
            vec![Ok(setup_responsive_daemon_server())],
            vec![
                Err(doctor_utils::DoctorUtilsError::ProcessStatusError),
                Err(doctor_utils::DoctorUtilsError::ProcessStatusError),
                Err(doctor_utils::DoctorUtilsError::ProcessStatusError),
            ],
        );
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        doctor_daemon_restart(&fake, DEFAULT_RETRY_DELAY, &mut ledger.root_guard()).await;

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
            \n[✓] Killing Daemon\
            \n    [!] Error getting daemon pid: <reason omitted>\
            \n    [✓] Killing zombie daemons.\
            \n[✓] Starting Daemon\
            \n    [✓] Daemon spawned\
            \n    [✓] Connected to daemon\
            \n    [✓] Daemon version: {DAEMON_VERSION}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n"
            )
        );
    }

    #[fuchsia::test]
    async fn test_single_try_daemon_running_no_targets_record_enabled() {
        let test_env = setup_test_env().await.unwrap();

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server())],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let (fake_recorder, params) = record_params_with_temp(root);

        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        let mock_driver_finder = default_mock_driver_finder();
        doctor(
            &mut handler,
            &mut ledger.root_guard(),
            false,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            params,
            mock_driver_finder,
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        let r = fake_recorder.lock().await;
        handler
            .assert_matches_steps(vec![
                TestStepEntry::output_step(StepType::DoctorSummaryInitVerbose),
                TestStepEntry::output_step(StepType::Output(format!(
                    "\
                    [✓] FFX doctor\
                    \n    [✓] Frontend version: {FRONTEND_VERSION}\
                    \n    [✓] abi-revision: {ABI_REVISION_STR}\
                    \n    [✓] api-level: {FAKE_API_LEVEL}\
                    \n    [i] Path to ffx: {ffx_path}\n\
                    \n[✓] FFX Environment Context\
                    \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
                    \n    [✓] Environment File Location: {env_file}\
                    \n    [✓] No build directory discovered in the environment.\
                    \n    [✓] Config Lock Files\
                    \n        [✓] {user_file} locked by {user_file}.lock\
                    \n        [✓] {global_file} locked by {global_file}.lock\
                    \n    [✓] The public & private Fuchsia keys are consistent\n\
                    \n[✓] FFX Emulator Instances\
                    \n    [i] No Emulator instances\n\
                    \n[✓] Checking daemon\
                    \n    [✓] Daemon found: [1]\
                    \n    [✓] Connecting to daemon\
                    \n    [✓] Daemon version: {DAEMON_VERSION}\
                    \n    [✓] path: {ffx_path}\
                    \n    [✓] abi-revision: {ABI_REVISION_STR}\
                    \n    [✓] api-level: {FAKE_API_LEVEL}\
                    \n    [✓] Default target: (none)\n\
                    \n[!] FFX USB Driver\
                    \n    [!] ffx-usb-driver is running.\
                    \n        [✓] PID: 1\
                    \n        [✓] Socket: /tmp/fake/socket/path\
                    \n        [!] ffx-usb-driver is listening on a different socket than what is configured: /tmp/fake/socket/path. Expected: {driver_socket_path}\n\
                    \n[✗] Google Network Checks\
                    \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
                    \n\n[✗] Searching for targets\
                    \n    [✗] No targets found!\n\
                    \n[✗] Doctor found issues in one or more categories.\n\n",
                    ffx_path=ffx_path(),
                    isolated_root=test_env.isolate_root.path().display(),
                    env_file=test_env.env_file.path().display(),
                    user_file=test_env.user_file.path().display(),
                    global_file=test_env.global_file.path().display(),
                    driver_socket_path =
                        test_env.isolate_root.path().join("test_usb_driver_socket/socket").display()
                ))),
                TestStepEntry::step(StepType::GeneratingRecord),
                TestStepEntry::result(StepResult::Success),
                TestStepEntry::output_step(StepType::RecordGenerated(FakeRecorder::result_path())),
            ])
            .await;
        r.assert_generate_called();
    }

    async fn missing_field_test(
        fake_recorder: Arc<Mutex<FakeRecorder>>,
        params: DoctorRecorderParameters,
    ) {
        let test_env = setup_test_env().await.unwrap();

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server())],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();

        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        let mock_driver_finder = default_mock_driver_finder();
        assert!(
            doctor(
                &mut handler,
                &mut ledger.root_guard(),
                false,
                &fake,
                "",
                1,
                DEFAULT_RETRY_DELAY,
                false,
                frontend_version_info(true),
                Ok(None),
                &test_env.context,
                params,
                mock_driver_finder,
                FakeGChecker,
                None,
                false,
            )
            .await
            .is_err()
        );

        let _ = fake_recorder.lock().await;
        handler
            .assert_matches_steps(vec![
                TestStepEntry::output_step(StepType::DoctorSummaryInitVerbose),
                TestStepEntry::output_step(StepType::Output(format!(
                    "\
                    [✓] FFX doctor\
                    \n    [✓] Frontend version: {FRONTEND_VERSION}\
                    \n    [✓] abi-revision: {ABI_REVISION_STR}\
                    \n    [✓] api-level: {FAKE_API_LEVEL}\
                    \n    [i] Path to ffx: {ffx_path}\n\
                    \n[✓] FFX Environment Context\
                    \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
                    \n    [✓] Environment File Location: {env_file}\
                    \n    [✓] No build directory discovered in the environment.\
                    \n    [✓] Config Lock Files\
                    \n        [✓] {user_file} locked by {user_file}.lock\
                    \n        [✓] {global_file} locked by {global_file}.lock\
                    \n    [✓] The public & private Fuchsia keys are consistent\n\
                    \n[✓] FFX Emulator Instances\
                    \n    [i] No Emulator instances\n\
                    \n[✓] Checking daemon\
                    \n    [✓] Daemon found: [1]\
                    \n    [✓] Connecting to daemon\
                    \n    [✓] Daemon version: {DAEMON_VERSION}\
                    \n    [✓] path: {ffx_path}\
                    \n    [✓] abi-revision: {ABI_REVISION_STR}\
                    \n    [✓] api-level: {FAKE_API_LEVEL}\
                    \n    [✓] Default target: (none)\n\
                    \n[!] FFX USB Driver\
                    \n    [!] ffx-usb-driver is running.\
                    \n        [✓] PID: 1\
                    \n        [✓] Socket: /tmp/fake/socket/path\
                    \n        [!] ffx-usb-driver is listening on a different socket than what is configured: /tmp/fake/socket/path. Expected: {driver_socket_path}\n\
                    \n[✗] Google Network Checks\
                    \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
                    \n\n[✗] Searching for targets\
                    \n    [✗] No targets found!\n\
                    \n[✗] Doctor found issues in one or more categories.\n\n",
                    ffx_path=ffx_path(),
                    isolated_root=test_env.isolate_root.path().display(),
                    env_file=test_env.env_file.path().display(),
                    user_file=test_env.user_file.path().display(),
                    global_file=test_env.global_file.path().display(),
                    driver_socket_path =
                        test_env.isolate_root.path().join("test_usb_driver_socket/socket").display()
                ))),
                // Error will occur here.
            ])
            .await;
        fake.assert_no_leftover_calls().await;
    }

    #[fuchsia::test]
    async fn test_record_mode_missing_log_root_fails() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let (fake_recorder, mut params) = record_params_with_temp(root);
        params.log_root = None;
        missing_field_test(fake_recorder, params).await;
    }

    #[fuchsia::test]
    async fn test_record_mode_missing_output_dir_fails() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let (fake_recorder, mut params) = record_params_with_temp(root);
        params.output_dir = None;
        missing_field_test(fake_recorder, params).await;
    }

    async fn test_finds_target_with_missing_nodename_setup(
        test_env: &TestEnv,
        mode: LedgerViewMode,
    ) -> DoctorLedger<MockWriter> {
        let (tx, rx) = oneshot::channel::<()>();

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server_with_targets(false, None, rx.shared()))],
            vec![Ok(vec![1])],
        );

        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(MockWriter::new(), ledger_view, mode);

        let mock_driver_finder = default_mock_driver_finder();
        doctor(
            &mut handler,
            &mut ledger.root_guard(),
            false,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            record_params_no_record(),
            mock_driver_finder,
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();
        tx.send(()).unwrap();

        return ledger;
    }

    #[fuchsia::test]
    async fn test_finds_target_with_missing_nodename_verbose() {
        let test_env = setup_test_env().await.unwrap();

        let ledger =
            test_finds_target_with_missing_nodename_setup(&test_env, LedgerViewMode::Verbose).await;
        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
                \n[✓] FFX doctor\
                \n    [✓] Frontend version: {FRONTEND_VERSION}\
                \n    [✓] abi-revision: {ABI_REVISION_STR}\
                \n    [✓] api-level: {FAKE_API_LEVEL}\
                \n    [i] Path to ffx: {ffx_path}\
                \n[✓] FFX Environment Context\
                \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
                \n    [✓] Environment File Location: {env_file}\
                \n    [✓] No build directory discovered in the environment.\
                \n    [✓] Config Lock Files\
                \n        [✓] {user_file} locked by {user_file}.lock\
                \n        [✓] {global_file} locked by {global_file}.lock\
                \n    [✓] The public & private Fuchsia keys are consistent\
                \n[✓] FFX Emulator Instances\
                \n    [i] No Emulator instances\
                \n[✓] Checking daemon\
                \n    [✓] Daemon found: [1]\
                \n    [✓] Connecting to daemon\
                \n    [✓] Daemon version: {DAEMON_VERSION}\
                \n    [✓] path: {ffx_path}\
                \n    [✓] abi-revision: {ABI_REVISION_STR}\
                \n    [✓] api-level: {FAKE_API_LEVEL}\
                \n    [✓] Default target: (none)\
                \n[!] FFX USB Driver\
                \n    [!] ffx-usb-driver is running.\
                \n        [✓] PID: 1\
                \n        [✓] Socket: /tmp/fake/socket/path\
                \n        [!] ffx-usb-driver is listening on a different socket than what is configured: /tmp/fake/socket/path. Expected: {driver_socket_path}\
                \n[✗] Google Network Checks\
                \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
                \n[✓] Searching for targets\
                \n    [✓] 2 targets found\
                \n[✗] Target: <unknown>\
                \n    [!] Compatibility state: absent\
                \n        [!] Compatibility information is not available\
                \n    [✓] Opened target handle\
                \n    [✓] Connecting to RCS\
                \n    [✗] Timeout while communicating with RCS\
                \n[✗] Target: {UNRESPONSIVE_NODENAME}\
                \n    [!] Compatibility state: absent\
                \n        [!] Compatibility information is not available\
                \n    [✓] Opened target handle\
                \n    [✓] Connecting to RCS\
                \n    [✗] Timeout while communicating with RCS\
                \n[✗] Doctor found issues in one or more categories.\n",
                ffx_path = ffx_path(),
                isolated_root = test_env.isolate_root.path().display(),
                env_file = test_env.env_file.path().display(),
                user_file = test_env.user_file.path().display(),
                global_file = test_env.global_file.path().display(),
                driver_socket_path =
                    test_env.isolate_root.path().join("test_usb_driver_socket/socket").display()
            )
        );
    }

    #[fuchsia::test]
    async fn test_finds_target_with_missing_nodename_normal() {
        let test_env = setup_test_env().await.unwrap();

        let ledger =
            test_finds_target_with_missing_nodename_setup(&test_env, LedgerViewMode::Normal).await;
        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
                \n[✓] FFX doctor\
                \n    [i] Path to ffx: {ffx_path}\
                \n[✓] FFX Environment Context\
                \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
                \n    [✓] Config Lock Files\
                \n        [✓] {user_file} locked by {user_file}.lock\
                \n        [✓] {global_file} locked by {global_file}.lock\
                \n    [✓] The public & private Fuchsia keys are consistent\
                \n[✓] FFX Emulator Instances\
                \n    [i] No Emulator instances\
                \n[✓] Checking daemon\
                \n    [✓] Daemon found: [1]\
                \n    [✓] Connecting to daemon\
                \n[!] FFX USB Driver\
                \n    [!] ffx-usb-driver is running.\
                \n        [!] ffx-usb-driver is listening on a different socket than what is configured: /tmp/fake/socket/path. Expected: {driver_socket_path}\
                \n[✗] Google Network Checks\
                \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
                \n[✓] Searching for targets\
                \n    [✓] 2 targets found\
                \n[✗] Target: <unknown>\
                \n[✗] Target: {UNRESPONSIVE_NODENAME}\
                \n[✗] Doctor found issues in one or more categories; \
                run 'ffx doctor -v' for more details.\n",
                ffx_path = ffx_path(),
                isolated_root = test_env.isolate_root.path().display(),
                user_file = test_env.user_file.path().display(),
                global_file = test_env.global_file.path().display(),
                driver_socket_path =
                    test_env.isolate_root.path().join("test_usb_driver_socket/socket").display()
            )
        );
    }

    #[fuchsia::test]
    async fn test_fastboot_target() {
        let test_env = setup_test_env().await.unwrap();

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server_with_fastboot_target())],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        let mock_driver_finder = default_mock_driver_finder();
        doctor(
            &mut handler,
            &mut ledger.root_guard(),
            false,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            record_params_no_record(),
            mock_driver_finder,
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
            \n[✓] FFX doctor\
            \n    [✓] Frontend version: {FRONTEND_VERSION}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [i] Path to ffx: {ffx_path}\
            \n[✓] FFX Environment Context\
            \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
            \n    [✓] Environment File Location: {env_file}\
            \n    [✓] No build directory discovered in the environment.\
            \n    [✓] Config Lock Files\
            \n        [✓] {user_file} locked by {user_file}.lock\
            \n        [✓] {global_file} locked by {global_file}.lock\
            \n    [✓] The public & private Fuchsia keys are consistent\
            \n[✓] FFX Emulator Instances\
            \n    [i] No Emulator instances\
            \n[✓] Checking daemon\
            \n    [✓] Daemon found: [1]\
            \n    [✓] Connecting to daemon\
            \n    [✓] Daemon version: {DAEMON_VERSION}\
            \n    [✓] path: {ffx_path}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [✓] Default target: (none)\
            \n[!] FFX USB Driver\
            \n    [!] ffx-usb-driver is running.\
            \n        [✓] PID: 1\
            \n        [✓] Socket: /tmp/fake/socket/path\
            \n        [!] ffx-usb-driver is listening on a different socket than what is configured: /tmp/fake/socket/path. Expected: {driver_socket_path}\
            \n[✗] Google Network Checks\
            \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
            \n[✓] Searching for targets\
            \n    [✓] 1 targets found\
            \n[✓] Target found in fastboot mode: {SERIAL_NUMBER}\
            \n[✓] No issues found\n",
                ffx_path = ffx_path(),
                isolated_root = test_env.isolate_root.path().display(),
                env_file = test_env.env_file.path().display(),
                user_file = test_env.user_file.path().display(),
                global_file = test_env.global_file.path().display(),
                driver_socket_path =
                    test_env.isolate_root.path().join("test_usb_driver_socket/socket").display()
            )
        );
    }

    #[fuchsia::test]
    async fn test_single_try_daemon_running_different_api_level() {
        let test_env = setup_test_env().await.unwrap();

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server())],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        let mock_driver_finder = default_mock_driver_finder();
        doctor(
            &mut handler,
            &mut ledger.root_guard(),
            false,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(false),
            Ok(Some("".to_string())),
            &test_env.context,
            record_params_no_record(),
            mock_driver_finder,
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
                   \n[✓] FFX doctor\
                   \n    [✓] Frontend version: {FRONTEND_VERSION}\
                   \n    [✓] abi-revision: {ABI_REVISION_STR}\
                   \n    [✓] api-level: {ANOTHER_FAKE_API_LEVEL}\
                   \n    [i] Path to ffx: {ffx_path}\
                   \n[✓] FFX Environment Context\
                   \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
                   \n    [✓] Environment File Location: {env_file}\
                   \n    [✓] No build directory discovered in the environment.\
                   \n    [✓] Config Lock Files\
                   \n        [✓] {user_file} locked by {user_file}.lock\
                   \n        [✓] {global_file} locked by {global_file}.lock\
                   \n    [✓] The public & private Fuchsia keys are consistent\
                   \n[✓] FFX Emulator Instances\
                   \n    [i] No Emulator instances\
                   \n[✓] Checking daemon\
                   \n    [✓] Daemon found: [1]\
                   \n    [✓] Connecting to daemon\
                   \n    [✓] Daemon version: {DAEMON_VERSION}\
                   \n    [✓] path: {ffx_path}\
                   \n    [✓] abi-revision: {ABI_REVISION_STR}\
                   \n    [✓] api-level: {FAKE_API_LEVEL}\
                   \n    [!] Daemon and frontend are at different API levels. Run `ffx doctor --restart-daemon`\
                   \n    [✓] Default target: (none)\
                   \n[!] FFX USB Driver\
                   \n    [!] ffx-usb-driver is running.\
                   \n        [✓] PID: 1\
                   \n        [✓] Socket: /tmp/fake/socket/path\
                   \n        [!] ffx-usb-driver is listening on a different socket than what is configured: /tmp/fake/socket/path. Expected: {driver_socket_path}\
                   \n[✗] Google Network Checks\
                   \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
                   \n[✗] Searching for targets\
                   \n    [✗] No targets found!\
                   \n[✗] Doctor found issues in one or more categories.\n",
                ffx_path = ffx_path(),
                isolated_root = test_env.isolate_root.path().display(),
                env_file = test_env.env_file.path().display(),
                user_file = test_env.user_file.path().display(),
                global_file = test_env.global_file.path().display(),
                driver_socket_path =
                    test_env.isolate_root.path().join("test_usb_driver_socket/socket").display()
            )
        );
    }

    #[fuchsia::test]
    async fn test_missing_ssh_keys() {
        let mut builder = ffx_config::test_env();
        let isolate_root = builder.isolate_root();
        let pub_key = isolate_root.join("test_authorized_keys");
        let priv_key = isolate_root.join("test_ed25519_key");

        let socket_file = setup_driver_socket_file(&isolate_root)
            .await
            .expect("setting up fake driver socket file");

        let test_env = builder
            .user_config("ssh.pub", json!([&pub_key]))
            .user_config("ssh.priv", json!([&priv_key]))
            .user_config(usb_driver_api::CONFIG_USB_SOCKET_PATH, json!(socket_file))
            .user_config(ffx_config::keys::USB_ENABLED, json!(true))
            .build()
            .unwrap();
        // Do not generate the keys - so they are missing.

        let fake = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server_with_fastboot_target())],
            vec![Ok(vec![1])],
        );
        let mut handler = FakeStepHandler::new();
        let ledger_view = Box::new(FakeLedgerView::new());
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            ledger_view,
            LedgerViewMode::Verbose,
        );

        let mock_driver_finder = default_mock_driver_finder();
        doctor(
            &mut handler,
            &mut ledger.root_guard(),
            false,
            &fake,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            record_params_no_record(),
            mock_driver_finder,
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            ledger.writer.get_data(),
            format!(
                "\
            \n[✓] FFX doctor\
            \n    [✓] Frontend version: {FRONTEND_VERSION}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [i] Path to ffx: {ffx_path}\
            \n[✗] FFX Environment Context\
            \n    [✓] Kind of Environment: Isolated environment with an isolated root of {isolated_root}\
            \n    [✓] Environment File Location: {env_file}\
            \n    [✓] No build directory discovered in the environment.\
            \n    [✓] Config Lock Files\
            \n        [✓] {user_file} locked by {user_file}.lock\
            \n        [✓] {global_file} locked by {global_file}.lock\
            \n    [✗] Private key {priv_key} does not exist. Check configuration or run `ffx doctor --repair-keys`\
            \n[✓] FFX Emulator Instances\
            \n    [i] No Emulator instances\
            \n[✓] Checking daemon\
            \n    [✓] Daemon found: [1]\
            \n    [✓] Connecting to daemon\
            \n    [✓] Daemon version: {DAEMON_VERSION}\
            \n    [✓] path: {ffx_path}\
            \n    [✓] abi-revision: {ABI_REVISION_STR}\
            \n    [✓] api-level: {FAKE_API_LEVEL}\
            \n    [✓] Default target: (none)\
            \n[!] FFX USB Driver\
            \n    [!] ffx-usb-driver is running.\
            \n        [✓] PID: 1\
            \n        [✓] Socket: /tmp/fake/socket/path\
            \n        [!] ffx-usb-driver is listening on a different socket than what is configured: /tmp/fake/socket/path. Expected: {driver_socket_path}\
            \n[✗] Google Network Checks\
            \n    [✗] Google-corp tool missing, please run `fx add-internal-tools` and `fx build --host //vendor/google/tools/gdoctor`\
            \n[✓] Searching for targets\
            \n    [✓] 1 targets found\
            \n[✓] Target found in fastboot mode: {SERIAL_NUMBER}\
            \n[✓] No issues found\n",
                ffx_path = ffx_path(),
                isolated_root = test_env.isolate_root.path().display(),
                env_file = test_env.env_file.path().display(),
                user_file = test_env.user_file.path().display(),
                global_file = test_env.global_file.path().display(),
                priv_key = priv_key.display(),
                driver_socket_path =
                    test_env.isolate_root.path().join("test_usb_driver_socket/socket").display()
            )
        );
    }

    #[test]
    fn test_collect_log_files() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let mut expected = vec![root.join("f1.log"), root.join("f2.log")];
        for p in &expected {
            fs::write(p, "something").expect("written testdata");
        }
        // write out other files
        fs::write(root.join("no_extension"), "something").expect("written testdata");
        fs::write(root.join("notlog.txt"), "something").expect("written testdata");
        fs::write(root.join("save.log.save"), "something").expect("written testdata");

        let subdir = root.join("subdir");
        fs::create_dir_all(&subdir).expect("subdir created");
        fs::write(subdir.join("sublog.log"), "something").expect("written testdata");

        let mut actual = collect_log_files(root.clone()).expect("collecting");
        // Sort the lists to make comparison easy.
        expected.sort();
        actual.sort();
        assert_eq!(expected, actual);
    }

    #[fuchsia::test]
    async fn test_doctor_summary_with_gdoctor_subtool() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let subtool_search_dir_path = temp_dir.path();
        let mock_gdoctor_path = temp_dir.path().join("ffx-gdoctor");
        // Scope to ensure File handle is dropped (and file closed) before setting permissions
        {
            let mut mock_gdoctor_script =
                fs::File::create(&mock_gdoctor_path).expect("Failed to create mock script");
            // example data in DoctorCheck format
            write!(
            mock_gdoctor_script,
            "#!/bin/sh\n\
             echo '{{\"name\": \"Corp DHCP\", \"message\": \"Successfully connected\", \"result\": \"passed\"}}'\n\
             echo '{{\"name\": \"GPN\", \"message\": \"GPN not detected\", \"result\": \"failed\"}}'\n"
        )
        .expect("Failed to write to mock script");
        }

        let mut perms = fs::metadata(&mock_gdoctor_path)
            .expect("Failed to get metadata for mock script")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&mock_gdoctor_path, perms)
            .expect("Failed to set permissions on mock script");
        let metadata_path = temp_dir.path().join("ffx-gdoctor.json");
        let metadata_content = serde_json::json!({
            "name": "gdoctor",
            "description": "Mock gdoctor for testing",
            "requires_fho": 0,
            "fho_details": {
                "version": 0
            }
        });
        fs::write(&metadata_path, metadata_content.to_string()).expect("Failed to write metadata");

        // Configure ffx to use our temporary search path
        let mut builder = ffx_config::test_env();
        let isolate_root = builder.isolate_root();
        let socket_file = setup_driver_socket_file(&isolate_root)
            .await
            .expect("setting up fake driver socket file");

        let test_env = builder
            .env_var(EnvironmentContext::FFX_BIN_ENV, "host-tools/ffx")
            .runtime_config("ffx.subtool-search-paths", json!([subtool_search_dir_path]))
            .user_config(usb_driver_api::CONFIG_USB_SOCKET_PATH, json!(socket_file))
            .build()
            .expect("Setting up test environment");

        let fake_daemon = FakeDaemonManager::new(
            vec![true],
            vec![],
            vec![],
            vec![Ok(setup_responsive_daemon_server())],
            vec![Ok(vec![123])],
        );
        let mut handler = FakeStepHandler::new();
        let mut ledger = DoctorLedger::<MockWriter>::new(
            MockWriter::new(),
            Box::new(FakeLedgerView::new()),
            LedgerViewMode::Verbose,
        );

        let mock_driver_finder = default_mock_driver_finder();
        doctor(
            &mut handler,
            &mut ledger.root_guard(),
            false,
            &fake_daemon,
            "",
            1,
            DEFAULT_RETRY_DELAY,
            false,
            frontend_version_info(true),
            Ok(None),
            &test_env.context,
            record_params_no_record(),
            mock_driver_finder,
            FakeGChecker,
            None,
            false,
        )
        .await
        .unwrap();

        let output = ledger.writer.get_data();
        assert!(
            output.contains("[✗] Google Network Checks"),
            "Main 'Google Network Checks' node missing or has wrong outcome. Output:\n{}",
            output
        );
        assert!(
            output.contains("[✓] Corp DHCP: Successfully connected"),
            "'Corp DHCP' check missing or has wrong outcome. Output:\n{}",
            output
        );
        assert!(
            output.contains("[✗] GPN: GPN not detected"),
            "'GPN' check missing or has wrong outcome. Output:\n{}",
            output
        );
    }

    #[fuchsia::test]
    async fn test_check_emulators() -> Result<()> {
        let mut builder = ffx_config::test_env();
        let isolate_root = builder.isolate_root();

        let socket_file = setup_driver_socket_file(&isolate_root)
            .await
            .expect("setting up fake driver socket file");
        let emu_dir = setup_emu_dir(&isolate_root).expect("setting up emulator data");

        let test_env = builder
            .user_config(usb_driver_api::CONFIG_USB_SOCKET_PATH, json!(socket_file))
            .user_config(ffx_config::keys::EMU_INSTANCE_ROOT_DIR, json!(&emu_dir))
            .build()
            .unwrap();
        // No instances
        {
            let mut writer = MockWriter::new();
            let mut ledger = DoctorLedger::new(
                &mut writer,
                Box::new(VisualLedgerView::new()),
                LedgerViewMode::Verbose,
            );
            check_emulators(&mut ledger.root_guard(), &test_env.context).await?;
            let output = writer.get_data();
            assert!(output.contains("FFX Emulator Instances"));
            assert!(!output.contains("Name:"), "got instance on empty dir: {}", output);
        }

        // One running instance
        let instance_dir = emu_dir.as_path().join("fuchsia-emulator");
        fs::create_dir(&instance_dir)?;
        let mut instance_data =
            EmulatorInstanceData::new_with_state("fuchsia-emulator", EngineState::Running);
        instance_data.set_pid(std::process::id());
        let engine_json_path = instance_dir.join("engine.json");
        fs::write(&engine_json_path, serde_json::to_string(&instance_data)?)?;

        {
            let mut writer = MockWriter::new();
            let mut ledger = DoctorLedger::new(
                &mut writer,
                Box::new(VisualLedgerView::new()),
                LedgerViewMode::Verbose,
            );
            check_emulators(&mut ledger.root_guard(), &test_env.context).await?;
            let output = writer.get_data();
            assert!(output.contains("FFX Emulator Instances"));
            assert!(output.contains("Name: fuchsia-emulator"));
            assert!(output.contains("Is Running: true"));
            assert!(output.contains("Engine State: running"));
        }

        // One stopped instance
        instance_data.set_engine_state(EngineState::Staged);
        instance_data.set_pid(0);
        fs::write(&engine_json_path, serde_json::to_string(&instance_data)?)?;

        {
            let mut writer = MockWriter::new();
            let mut ledger = DoctorLedger::new(
                &mut writer,
                Box::new(VisualLedgerView::new()),
                LedgerViewMode::Verbose,
            );
            check_emulators(&mut ledger.root_guard(), &test_env.context).await?;
            let output = writer.get_data();
            assert!(output.contains("FFX Emulator Instances"));
            assert!(output.contains("Name: fuchsia-emulator"));
            assert!(output.contains("Is Running: false"));
            assert!(output.contains("Engine State: staged"));
        }

        Ok(())
    }
}
