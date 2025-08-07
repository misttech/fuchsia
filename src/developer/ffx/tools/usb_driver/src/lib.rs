// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use argh::{ArgsInfo, FromArgs, SubCommand};
use fho::subtool::{StandaloneFhoHandler, StandaloneToolCommand};
use fho::{FfxContext, Result};
use std::fs::OpenOptions;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};

/// Config element for the path of the socket we will use to communicate.
const USB_SOCKET_PATH_CONFIG: &str = "connectivity.usb_socket_path";

/// Default name for the control socket.
const USB_SOCKET_NAME: &str = "ffx_usb.sock";

// [START command_struct]
#[derive(ArgsInfo, FromArgs, Debug, PartialEq)]
#[argh(subcommand, name = "usb-driver")]
/// Start a long-running process to drive USB-connected Fuchsia devices and
/// allow them to be connected to from FFX.
pub struct UsbDriverCommand {
    #[argh(switch)]
    /// whether to fork the driver process into the background rather than run
    background: bool,
}
// [END command_struct]

pub async fn run() {
    let mut logging_enabled = false;
    let result = implementation(&mut logging_enabled).await;
    let should_format = match fho::FfxCommandLine::from_env() {
        Ok(cli) => cli.global.machine.is_some(),
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
    ffx_command::exit(result, should_format).await;
}

async fn implementation(logging_enabled: &mut bool) -> Result<ExitStatus> {
    let ffx_command::InitializedCmd { cmd: ffx, context: ctx, help_state } =
        ffx_command::init_cmd(ffx_config::environment::ExecutableKind::Subtool)?;

    let log_id: u64 = rand::random();

    match help_state {
        ffx_command::HelpState::ReturnArgsInfo => {
            let args_info = ffx_command::CliArgsInfo::from(UsbDriverCommand::get_args_info());
            let output = match ffx.global.machine.unwrap() {
                ffx_command::MachineFormat::Json => serde_json::to_string(&args_info),
                ffx_command::MachineFormat::JsonPretty => serde_json::to_string_pretty(&args_info),
                ffx_command::MachineFormat::Raw => Ok(format!("{args_info:#?}")),
            };
            println!("{}", output.bug_context("Error serializing args")?);
            return Ok(ExitStatus::from_raw(0));
        }
        ffx_command::HelpState::ReturnHelp { command, output, code } => {
            return Err(fho::Error::Help { command, output, code })
        }
        ffx_command::HelpState::None => (),
    }

    let args = Vec::from_iter(ffx.global.subcommand.iter().map(String::as_str));
    let command = StandaloneToolCommand::<UsbDriverCommand>::from_args(
        &Vec::from_iter(ffx.cmd_iter()),
        &args,
    )
    .map_err(|err| ffx_command::Error::from_early_exit(&ffx.command, err))?;

    let command = match command.subcommand {
        StandaloneFhoHandler::Metadata(metadata_cmd) => {
            return metadata_cmd.run(UsbDriverCommand::COMMAND).await;
        }
        StandaloneFhoHandler::Standalone(cmd) => cmd,
    };

    let sink = if command.background {
        let mut path = match ffx_config::get_state_base_path() {
            Ok(p) => p,
            Err(e) => return Err(ffx_command::Error::Config(e.into())),
        };

        path.push("ffx_usb");
        path.push(format!("ffx_usb.{log_id:x}.log"));

        let file = match OpenOptions::new().write(true).append(true).create(true).open(path) {
            Ok(f) => f,
            Err(e) => {
                return Err(ffx_command::Error::Config(anyhow::anyhow!(
                    "Could not open log file: {e}"
                )));
            }
        };

        Box::new(logging::FfxLogSink::new(Arc::new(Mutex::new(file))))
            as Box<dyn logging::LogSinkTrait>
    } else {
        Box::new(logging::FfxLogSink::new(Arc::new(Mutex::new(std::io::stderr()))))
            as Box<dyn logging::LogSinkTrait>
    };

    struct Filter;
    impl logging::Filter for Filter {
        fn should_emit(&self, _record: &log::Metadata<'_>) -> bool {
            true
        }
    }

    let logger = logging::FfxLog::new(
        vec![sink],
        logging::FormatOpts::new(0),
        Filter,
        log::LevelFilter::Debug,
        logging::TargetsFilter::new(vec![]),
    );

    let _ = log::set_boxed_logger(Box::new(logger))
        .map(|()| log::set_max_level(log::LevelFilter::Trace));
    *logging_enabled = true;

    if ffx.global.schema {
        todo!();
    }

    let (socket_path, found_config) = ffx_config::build()
        .context(Some(&ctx))
        .level(Some(ffx_config::ConfigLevel::Runtime))
        .name(Some(USB_SOCKET_PATH_CONFIG))
        .get::<PathBuf>()
        .map(|x| (x, true))
        .or_else(|_| -> fho::Result<_> {
            let runtime_dir =
                std::env::var("XDG_RUNTIME_DIR").ok().filter(|x| !x.is_empty()).ok_or_else(
                    || fho::Error::Unexpected(anyhow::anyhow!("$XDG_RUNTIME_DIR is not set")),
                )?;

            let mut ret = PathBuf::from(runtime_dir);
            ret.push(USB_SOCKET_NAME);
            Ok((ret, false))
        })?;

    if !found_config
        && ffx_config::build()
            .context(Some(&ctx))
            .name(Some(USB_SOCKET_PATH_CONFIG))
            .get::<String>()
            .is_ok()
    {
        return Err(fho::Error::User(anyhow::anyhow!(
            "{USB_SOCKET_PATH_CONFIG} must only be set on the command line"
        )));
    }

    if command.background {
        // daemonize(3) is deprecated on macOS 10.15. The replacement is not
        // yet clear, we may want to replace this with a manual double fork
        // setsid, etc.
        #[allow(deprecated)]
        // First argument: chdir(/)
        // Second argument: close stdio
        //
        // SAFETY: This shouldn't do much of anything to memory state. If it
        // succeeds we've effectively just been shuffled around the process
        // table. If it fails then it likely has no side effects at all, but
        // even if it does we're going to exit as fast as we can anyway.
        match unsafe { libc::daemon(0, 0) } {
            0 => (),
            x => return Err(fho::Error::Unexpected(std::io::Error::from_raw_os_error(x).into())),
        }
    }

    usb_driver_impl::HostDriver::run(socket_path).await;
    Ok(ExitStatus::from_raw(0))
}
