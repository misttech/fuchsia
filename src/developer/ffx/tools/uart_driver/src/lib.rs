// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Standalone daemon execution framework for the Fuchsia UART host driver.
//!
//! Provides the command line entry point, background daemon detachment supervisor,
//! log redirection and rotation, and socket lifecycle management for `ffx-uart-driver`.

use argh::{ArgsInfo, FromArgs, SubCommand};
use ffx_config::EnvironmentContext;
use fho::subtool::{StandaloneFhoHandler, StandaloneToolCommand};
use fho::{FfxCommandLine, Result};
use std::fs::OpenOptions;
use std::num::NonZeroU32;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};

mod analytics;

/// Default serial baud rate (1,000,000 baud).
pub const DEFAULT_BAUD_RATE: NonZeroU32 = match NonZeroU32::new(1_000_000) {
    Some(v) => v,
    None => unreachable!(),
};

/// Number of log file rotations to keep.
const LOG_ROTATIONS: usize = 5;

/// Command line options for the `ffx-uart-driver` daemon executable.
#[derive(ArgsInfo, FromArgs, Clone, Debug, PartialEq)]
#[argh(subcommand, name = "uart-driver", description = "Driver daemon for UART transport")]
pub struct UartDriverCommand {
    #[argh(switch)]
    /// run the driver in the background
    background: bool,

    #[argh(option)]
    /// directory where log files should be stored
    log_dir: Option<PathBuf>,

    #[argh(option)]
    /// target UART port path (TTY or UNIX socket)
    target: String,

    #[argh(option)]
    /// path to the UNIX socket where the driver will listen for clients
    socket: Option<PathBuf>,

    #[argh(option, default = "DEFAULT_BAUD_RATE")]
    /// baud rate for the UART port (only used for TTY targets)
    baud: NonZeroU32,

    #[argh(switch)]
    /// do not retry connection if it fails or is lost
    no_retry: bool,
}

impl UartDriverCommand {
    /// Serializes the parsed command options back into command-line argument strings.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec![Self::COMMAND.name.to_string()];
        if self.background {
            args.push("--background".to_string());
        }
        if let Some(ref log_dir) = self.log_dir {
            args.push("--log-dir".to_string());
            args.push(log_dir.display().to_string());
        }
        args.push("--target".to_string());
        args.push(self.target.clone());
        if let Some(ref socket) = self.socket {
            args.push("--socket".to_string());
            args.push(socket.display().to_string());
        }
        args.push("--baud".to_string());
        args.push(self.baud.to_string());
        if self.no_retry {
            args.push("--no-retry".to_string());
        }
        args
    }
}

/// Main execution entry point for the `ffx-uart-driver` host tool binary.
///
/// Initializes the FFX command environment, parses command line arguments into
/// [`UartDriverCommand`], configures logging and log file rotation, and either
/// spawns a detached background daemon process or executes the driver event loop
/// in the foreground.
pub async fn run() {
    let mut env_context = None;
    let mut logging_enabled = false;
    let result = match ffx_command::init_cmd(ffx_config::environment::ExecutableKind::Subtool) {
        Ok(c) => {
            env_context = Some(c.context.clone());
            Box::pin(implementation(c, &mut logging_enabled)).await
        }
        Err(e) => Err(e),
    };
    let should_format = match fho::FfxCommandLine::from_env() {
        Ok(cli) => cli.global.should_format(),
        Err(e) => {
            if logging_enabled {
                log::warn!("Received error getting command line: {}", e);
            } else {
                eprintln!("Received error getting command line: {}", e);
            }
            match e {
                fho::Error::Help { .. } => false,
                _ => true,
            }
        }
    };
    ffx_command::exit(env_context, result, should_format).await;
}

fn resolve_socket_path(
    ctx: &EnvironmentContext,
    command_socket: Option<&Path>,
    target: &str,
) -> Result<PathBuf, ffx_command::Error> {
    let path = if let Some(sock) = command_socket {
        sock.to_path_buf()
    } else {
        uart_driver_api::get_socket_path_from_target_path(Path::new(target), ctx).map_err(|e| {
            ffx_command::Error::Config(
                std::io::Error::new(std::io::ErrorKind::Other, e.to_string()).into(),
            )
        })?
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    Ok(path)
}

fn rotate_and_open_log_file(
    log_dir_path: &Path,
    log_id: u64,
) -> std::io::Result<(std::fs::File, String)> {
    let _ = std::fs::create_dir_all(log_dir_path);
    for rot in (0..LOG_ROTATIONS).rev() {
        let current_log = uart_driver_api::get_driver_log_file_path(log_dir_path, log_id, rot);
        if rot + 1 == LOG_ROTATIONS {
            if let Err(e) = std::fs::remove_file(&current_log) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    log::warn!(
                        "Failed to remove rotated log file at {}: {:?}",
                        current_log.display(),
                        e
                    );
                }
            }
        } else {
            let next_log = uart_driver_api::get_driver_log_file_path(log_dir_path, log_id, rot + 1);
            let _ = std::fs::rename(current_log, next_log);
        }
    }
    let target_file_path = uart_driver_api::get_driver_log_file_path(log_dir_path, log_id, 0);
    let file = OpenOptions::new().write(true).append(true).create(true).open(&target_file_path)?;
    let log_path_str = target_file_path.to_string_lossy().to_string();
    Ok((file, log_path_str))
}

fn resolve_log_dir(
    ctx: &EnvironmentContext,
    log_dir: Option<&Path>,
) -> Result<PathBuf, ffx_command::Error> {
    if let Some(dir) = log_dir {
        Ok(dir.to_path_buf())
    } else {
        let mut p = match ctx.get::<PathBuf, _>("shared_data") {
            Ok(p) => p,
            Err(_) => {
                ctx.get_shared_data_path().map_err(|e| ffx_command::Error::Config(e.into()))?
            }
        };
        p.push(uart_driver_api::UART_SHARED_SUBDIR);
        Ok(p)
    }
}

fn create_log_sink(
    ctx: &EnvironmentContext,
    command: &UartDriverCommand,
    log_id: u64,
) -> Result<(Box<dyn logging::LogSinkTrait>, String), ffx_command::Error> {
    if command.log_dir.is_some() {
        let path = resolve_log_dir(ctx, command.log_dir.as_deref())?;
        let (file, log_path_str) = rotate_and_open_log_file(&path, log_id).map_err(|e| {
            ffx_command::Error::Config(
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Could not open log file: {e}"),
                )
                .into(),
            )
        })?;
        Ok((Box::new(logging::FfxLogSink::new(Arc::new(Mutex::new(file)))), log_path_str))
    } else {
        Ok((
            Box::new(logging::FfxLogSink::new(Arc::new(Mutex::new(std::io::stderr())))),
            "stderr".to_owned(),
        ))
    }
}

fn init_logger(
    ctx: &EnvironmentContext,
    command: &UartDriverCommand,
    socket_path: &Path,
    invocation_id: u64,
    logging_enabled: &mut bool,
) -> Result<String, ffx_command::Error> {
    let log_id = uart_driver_api::get_log_id_from_socket_path(socket_path);
    let (sink, log_path) = create_log_sink(ctx, command, log_id)?;

    struct Filter;
    impl logging::Filter for Filter {
        fn should_emit(&self, _record: &log::Metadata<'_>) -> bool {
            true
        }
    }

    let log_level = ctx
        .get::<String, _>("log.level")
        .ok()
        .and_then(|s| s.to_lowercase().parse::<log::LevelFilter>().ok())
        .unwrap_or(log::LevelFilter::Info);

    let logger = logging::FfxLog::new(
        vec![sink],
        logging::FormatOpts::new(invocation_id),
        Filter,
        log_level,
        logging::TargetsFilter::new(vec![]),
    );

    let _ = log::set_boxed_logger(Box::new(logger)).map(|()| log::set_max_level(log_level));
    *logging_enabled = true;
    Ok(log_path)
}

/// Spawns the driver daemon as a detached background subprocess.
///
/// Re-executes the binary via `fork + exec` (`std::process::Command::spawn`)
/// with the `background` switch unset and `log_dir` cleared, redirecting standard I/O to the
/// pre-opened log file and placing the child in a new process group (`process_group(0)`).
///
/// Spawning a fresh subprocess rather than performing an in-place double fork
/// (`libc::daemon`) ensures the background driver starts in a clean address space
/// with newly initialized async runtimes and memory allocators, avoiding deadlocks
/// from locks held across `fork` in multi-threaded programs.
fn spawn_background_daemon(
    ffx: &FfxCommandLine,
    command: &UartDriverCommand,
    log_file: std::fs::File,
) -> Result<ExitStatus> {
    let current_exe = std::env::current_exe().map_err(|e| fho::Error::Unexpected(e.into()))?;
    let mut cmd = std::process::Command::new(current_exe);
    cmd.args(&ffx.ffx_args);

    let mut child_command = command.clone();
    child_command.background = false;
    // Strip `--log-dir` because the supervisor has already rotated the logs
    // and redirected the child's stdout/stderr to the `.0.log` file; passing
    // `--log-dir` to the child would cause `create_log_sink` to rotate again.
    child_command.log_dir = None;
    cmd.args(child_command.to_args());

    cmd.stdin(std::process::Stdio::null());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    let log_file_err = log_file.try_clone().map_err(|e| fho::Error::Unexpected(e.into()))?;
    cmd.stdout(log_file);
    cmd.stderr(log_file_err);

    let _child = cmd.spawn().map_err(|e| fho::Error::Unexpected(e.into()))?;
    Ok(ExitStatus::from_raw(0))
}

async fn run_driver_daemon(
    ctx: &EnvironmentContext,
    command: UartDriverCommand,
    socket_path: PathBuf,
) -> Result<ExitStatus> {
    let listener = match uart_driver_impl::remove_and_bind_socket(socket_path.clone(), true).await {
        Ok(l) => l,
        Err(uart_driver_impl::RemoveAndBindError::InUse(_)) => {
            log::warn!(
                "UART driver daemon is already running on socket: {}",
                socket_path.display()
            );
            return Ok(ExitStatus::from_raw(0));
        }
        Err(e) => {
            return Err(fho::user_error!("Failed to bind socket {}: {e}", socket_path.display()));
        }
    };

    let pid = std::process::id();
    let meta_path = Some(uart_driver_api::get_metadata_path(&socket_path));

    ffx_tool_uart::write_metadata(ctx, &socket_path, &command.target, pid, Some(command.baud))
        .map_err(|e| fho::bug!("Failed to write connection metadata: {:?}", e))?;

    log::info!("UART driver listening on socket: {}", socket_path.display());
    uart_driver_impl::HostDriver::run(
        listener,
        command.target,
        command.baud,
        meta_path,
        command.no_retry,
    )
    .await;
    Ok(ExitStatus::from_raw(0))
}

enum ParsedToolCommand {
    EarlyExit(ExitStatus),
    Run(UartDriverCommand),
}

async fn parse_tool_command(ffx: &FfxCommandLine) -> Result<ParsedToolCommand, ffx_command::Error> {
    let args = Vec::from_iter(ffx.global.subcommand.iter().map(String::as_str));
    let command = StandaloneToolCommand::<UartDriverCommand>::from_args(
        &Vec::from_iter(ffx.cmd_iter()),
        &args,
    )
    .map_err(|err| ffx_command::Error::from_early_exit(&ffx.command, err))?;

    match command.subcommand {
        StandaloneFhoHandler::Metadata(metadata_cmd) => {
            metadata_cmd.run(UartDriverCommand::COMMAND).await.map(ParsedToolCommand::EarlyExit)
        }
        StandaloneFhoHandler::Standalone(cmd) => Ok(ParsedToolCommand::Run(cmd)),
    }
}

async fn implementation(
    icmd: ffx_command::InitializedCmd,
    logging_enabled: &mut bool,
) -> Result<ExitStatus> {
    let ffx_command::InitializedCmd { cmd: ffx, context: ctx, help_state } = icmd;

    match help_state {
        ffx_command::HelpState::ReturnArgsInfo => {
            let args_info = ffx_command::CliArgsInfo::from(UartDriverCommand::get_args_info());
            let output = match ffx.global.machine.unwrap_or(ffx_command::MachineFormat::Json) {
                ffx_command::MachineFormat::Json => serde_json::to_string(&args_info),
                ffx_command::MachineFormat::JsonPretty => serde_json::to_string_pretty(&args_info),
                ffx_command::MachineFormat::Raw => Ok(format!("{args_info:#?}")),
            };
            println!("{}", output.map_err(|e| fho::Error::Unexpected(e.into()))?);
            return Ok(ExitStatus::from_raw(0));
        }
        ffx_command::HelpState::ReturnHelp { command, output, code } => {
            return Err(fho::Error::Help { command, output, code });
        }
        ffx_command::HelpState::None => (),
    }

    let command = match parse_tool_command(&ffx).await? {
        ParsedToolCommand::EarlyExit(status) => return Ok(status),
        ParsedToolCommand::Run(cmd) => cmd,
    };

    let metrics = ffx_command::MetricsSession::start(&ctx).await.ok();
    let invoker = ctx.get::<String, _>("fuchsia.analytics.ffx_invoker").ok();

    let res =
        execute_driver_command(&ctx, &ffx, &command, invoker.as_deref(), logging_enabled).await;

    if let Some(metrics) = metrics {
        let redacted_args = ffx.redact_subcmd(&command);
        let subcommand_chain = ffx.redact_subcmd_from_args_info::<UartDriverCommand>();
        let enhanced_args = match ffx_command::send_enhanced_analytics().await {
            true => Some(ffx.unredacted_args_for_analytics()),
            false => None,
        };
        let _ = metrics
            .command_finished(
                &res,
                &redacted_args,
                &subcommand_chain,
                false,
                enhanced_args.as_deref(),
            )
            .await;
    }

    res
}

async fn execute_driver_command(
    ctx: &EnvironmentContext,
    ffx: &FfxCommandLine,
    command: &UartDriverCommand,
    invoker: Option<&str>,
    logging_enabled: &mut bool,
) -> Result<ExitStatus> {
    let invocation_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    let socket_path = resolve_socket_path(ctx, command.socket.as_deref(), &command.target)?;

    if command.background {
        #[cfg(unix)]
        if UnixStream::connect(&socket_path).is_ok() {
            log::warn!(
                "UART driver daemon is already running on socket: {}",
                socket_path.display()
            );
            return Ok(ExitStatus::from_raw(0));
        }
        let log_dir = resolve_log_dir(ctx, command.log_dir.as_deref())?;
        let log_id = uart_driver_api::get_log_id_from_socket_path(&socket_path);
        let (file, _log_path) = rotate_and_open_log_file(&log_dir, log_id).map_err(|e| {
            ffx_command::Error::Config(
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Could not open log file: {e}"),
                )
                .into(),
            )
        })?;
        // Spawn the daemon child process before awaiting network telemetry so slow
        // analytics uploads cannot delay daemon socket creation.
        let res = spawn_background_daemon(ffx, command, file);
        analytics::emit_uart_driver_invoked_event(command, invoker).await;
        return res;
    }

    let _log_path = init_logger(ctx, command, &socket_path, invocation_id, logging_enabled)?;
    analytics::emit_uart_driver_invoked_event(command, invoker).await;
    run_driver_daemon(ctx, command.clone(), socket_path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_rotate_and_open_log_file() {
        let temp = tempfile::tempdir().unwrap();
        let log_dir = temp.path().join("logs");
        let log_id = 0xabcdef;

        let (file1, path1) = rotate_and_open_log_file(&log_dir, log_id).unwrap();
        drop(file1);
        let expected0 = uart_driver_api::get_driver_log_file_path(&log_dir, log_id, 0);
        assert_eq!(path1, expected0.to_string_lossy());
        assert!(expected0.exists());

        let (file2, path2) = rotate_and_open_log_file(&log_dir, log_id).unwrap();
        drop(file2);
        assert_eq!(path2, expected0.to_string_lossy());
        let expected1 = uart_driver_api::get_driver_log_file_path(&log_dir, log_id, 1);
        assert!(expected1.exists());
    }

    #[fuchsia::test]
    fn test_uart_driver_command_baud_parsing() {
        let cmd = UartDriverCommand::from_args(&["ffx-uart-driver"], &["--target", "/dev/ttyUSB0"])
            .unwrap();
        assert_eq!(cmd.baud, DEFAULT_BAUD_RATE);

        let cmd = UartDriverCommand::from_args(
            &["ffx-uart-driver"],
            &["--target", "/dev/ttyUSB0", "--baud", "115200"],
        )
        .unwrap();
        assert_eq!(cmd.baud, NonZeroU32::new(115200).unwrap());

        let err = UartDriverCommand::from_args(
            &["ffx-uart-driver"],
            &["--target", "/dev/ttyUSB0", "--baud", "0"],
        );
        assert!(err.is_err());
    }

    #[fuchsia::test]
    fn test_uart_driver_command_to_args() {
        let cmd = UartDriverCommand {
            background: false,
            log_dir: Some(PathBuf::from("/tmp/logs")),
            target: "/dev/ttyUSB0".to_string(),
            socket: Some(PathBuf::from("/tmp/socket")),
            baud: NonZeroU32::new(115200).unwrap(),
            no_retry: true,
        };
        let args = cmd.to_args();
        assert_eq!(
            args,
            vec![
                "uart-driver",
                "--log-dir",
                "/tmp/logs",
                "--target",
                "/dev/ttyUSB0",
                "--socket",
                "/tmp/socket",
                "--baud",
                "115200",
                "--no-retry",
            ]
        );

        let parsed = StandaloneToolCommand::<UartDriverCommand>::from_args(
            &["ffx-uart-driver"],
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        )
        .unwrap();
        if let StandaloneFhoHandler::Standalone(parsed_cmd) = parsed.subcommand {
            assert_eq!(parsed_cmd, cmd);
        } else {
            panic!("Expected Standalone subcommand");
        }
    }

    #[fuchsia::test]
    fn test_uart_driver_command_to_args_background() {
        let mut cmd = UartDriverCommand {
            background: true,
            log_dir: None,
            target: "/dev/ttyUSB0".to_string(),
            socket: None,
            baud: DEFAULT_BAUD_RATE,
            no_retry: false,
        };
        assert!(cmd.to_args().contains(&"--background".to_string()));
        cmd.background = false;
        assert!(!cmd.to_args().contains(&"--background".to_string()));
    }

    #[fuchsia::test]
    fn test_uart_driver_command_path_parsing() {
        let cmd = UartDriverCommand::from_args(
            &["ffx-uart-driver"],
            &[
                "--target",
                "/dev/ttyUSB0",
                "--socket",
                "/custom/socket.sock",
                "--log-dir",
                "/custom/logs",
            ],
        )
        .unwrap();
        assert_eq!(cmd.socket, Some(PathBuf::from("/custom/socket.sock")));
        assert_eq!(cmd.log_dir, Some(PathBuf::from("/custom/logs")));
    }
}
