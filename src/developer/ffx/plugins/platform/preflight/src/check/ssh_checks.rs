// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::check::PreflightCheckResult;
use crate::command_runner::CommandRunner;
use crate::config::{OperatingSystem, PreflightConfig};
use anyhow::Result;
use ffx_config::EnvironmentContext;

const SSH_NOT_FOUND_ERROR: &str = "Did not find an ssh binary on the path for the current user.";
const SSH_NOT_FOUND_RESOLUTION_MESSAGE: &str = "To resolve, run: sudo apt install ssh";
const SSH_CONFIG_NOT_FOUND_ERROR: &str = "Did not find an ssh config directory at ~/.ssh";
const SSH_CONFIG_NOT_FOUND_RESOLUTION_MESSAGE: &str = "To resolve, run: mkdir $HOME/.ssh";
const SUCCESS_MESSAGE: &str = "Found ssh binary and $HOME/.ssh directory.";

pub struct SshChecks<'a> {
    command_runner: &'a CommandRunner,
    context: &'a EnvironmentContext,
}

impl<'a> SshChecks<'a> {
    pub fn new(command_runner: &'a CommandRunner, context: &'a EnvironmentContext) -> Self {
        SshChecks { command_runner, context }
    }

    /// Returns Ok() if there is an ssh binary present and an ssh config directory in $HOME
    pub async fn run(&self, config: &PreflightConfig) -> Result<PreflightCheckResult> {
        let (ssh_status, _stdout, __stderr) = (self.command_runner)(&vec!["which", "ssh"])?;
        if !ssh_status.success() {
            return match &config.system {
                OperatingSystem::Linux => Ok(PreflightCheckResult::Failure(
                    SSH_NOT_FOUND_ERROR.to_string(),
                    Some(SSH_NOT_FOUND_RESOLUTION_MESSAGE.to_string()),
                )),
                OperatingSystem::MacOS(..) => {
                    Ok(PreflightCheckResult::Failure(SSH_NOT_FOUND_ERROR.to_string(), None))
                }
            };
        }
        let home = self.context.env_var("HOME")?;
        let (ssh_config_status, _stdout, _stderr) =
            (self.command_runner)(&vec!["test", "-d", &format!("{}/.ssh", home)])?;
        if !ssh_config_status.success() {
            return Ok(PreflightCheckResult::Failure(
                SSH_CONFIG_NOT_FOUND_ERROR.to_string(),
                Some(SSH_CONFIG_NOT_FOUND_RESOLUTION_MESSAGE.to_string()),
            ));
        }
        Ok(PreflightCheckResult::Success(SUCCESS_MESSAGE.to_string()))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::command_runner::ExitStatus;

    #[fuchsia::test]
    async fn test_linux_success() -> Result<()> {
        let env = ffx_config::test_env().env_var("HOME", "~").build()?;
        let run_command: CommandRunner = |args| {
            if args.to_vec() == vec!["test", "-d", "~/.ssh"] {
                return Ok((ExitStatus(0), "".to_string(), "".to_string()));
            }
            assert_eq!(args.to_vec(), vec!["which", "ssh"]);
            Ok((ExitStatus(0), "/usr/bin/ssh\n".to_string(), "".to_string()))
        };

        let check = SshChecks::new(&run_command, &env.context);
        let response = check.run(&PreflightConfig { system: OperatingSystem::Linux }).await;
        assert!(matches!(response?, PreflightCheckResult::Success(..)));
        Ok(())
    }

    #[fuchsia::test]
    async fn test_linux_no_ssh_binary() -> Result<()> {
        let env = ffx_config::test_env().env_var("HOME", "~").build()?;
        let run_command: CommandRunner = |args| {
            if args.to_vec() == vec!["test", "-d", "~/.ssh"] {
                unreachable!()
            }
            assert_eq!(args.to_vec(), vec!["which", "ssh"]);
            Ok((ExitStatus(1), "ssh not found\n".to_string(), "".to_string()))
        };

        let check = SshChecks::new(&run_command, &env.context);
        let response = check.run(&PreflightConfig { system: OperatingSystem::Linux }).await;
        assert!(matches!(response?, PreflightCheckResult::Failure(..)));
        Ok(())
    }

    #[fuchsia::test]
    async fn test_linux_no_ssh_config() -> Result<()> {
        let env = ffx_config::test_env().env_var("HOME", "~").build()?;
        let run_command: CommandRunner = |args| {
            if args.to_vec() == vec!["test", "-d", "~/.ssh"] {
                return Ok((ExitStatus(1), "".to_string(), "".to_string()));
            }
            assert_eq!(args.to_vec(), vec!["which", "ssh"]);
            Ok((ExitStatus(0), "/usr/bin/ssh\n".to_string(), "".to_string()))
        };

        let check = SshChecks::new(&run_command, &env.context);
        let response = check.run(&PreflightConfig { system: OperatingSystem::Linux }).await;
        assert!(matches!(response?, PreflightCheckResult::Failure(..)));
        Ok(())
    }

    #[fuchsia::test]
    async fn test_macos_success() -> Result<()> {
        let env = ffx_config::test_env().env_var("HOME", "~").build()?;
        let run_command: CommandRunner = |args| {
            if args.to_vec() == vec!["test", "-d", "~/.ssh"] {
                return Ok((ExitStatus(0), "".to_string(), "".to_string()));
            }
            assert_eq!(args.to_vec(), vec!["which", "ssh"]);
            Ok((ExitStatus(0), "/usr/local/bin/ssh\n".to_string(), "".to_string()))
        };

        let check = SshChecks::new(&run_command, &env.context);
        let response = check.run(&PreflightConfig { system: OperatingSystem::MacOS(10, 15) }).await;
        assert!(matches!(response?, PreflightCheckResult::Success(..)));
        Ok(())
    }

    #[fuchsia::test]
    async fn test_macos_no_ssh_binary() -> Result<()> {
        let env = ffx_config::test_env().env_var("HOME", "~").build()?;
        let run_command: CommandRunner = |args| {
            if args.to_vec() == vec!["test", "-d", "~/.ssh"] {
                unreachable!()
            }
            assert_eq!(args.to_vec(), vec!["which", "ssh"]);
            Ok((ExitStatus(1), "ssh not found\n".to_string(), "".to_string()))
        };

        let check = SshChecks::new(&run_command, &env.context);
        let response = check.run(&PreflightConfig { system: OperatingSystem::MacOS(10, 15) }).await;
        assert!(matches!(response?, PreflightCheckResult::Failure(..)));
        Ok(())
    }

    #[fuchsia::test]
    async fn test_macos_no_ssh_config() -> Result<()> {
        let env = ffx_config::test_env().env_var("HOME", "~").build()?;
        let run_command: CommandRunner = |args| {
            if args.to_vec() == vec!["test", "-d", "~/.ssh"] {
                return Ok((ExitStatus(1), "".to_string(), "".to_string()));
            }
            assert_eq!(args.to_vec(), vec!["which", "ssh"]);
            Ok((ExitStatus(0), "/usr/local/bin/ssh\n".to_string(), "".to_string()))
        };

        let check = SshChecks::new(&run_command, &env.context);
        let response = check.run(&PreflightConfig { system: OperatingSystem::MacOS(10, 15) }).await;
        assert!(matches!(response?, PreflightCheckResult::Failure(..)));
        Ok(())
    }
}
