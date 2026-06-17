// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use errors;
use ffx_config::EnvironmentContext;
use ffx_diagnostics::Notifier;
use ffx_wait_args::WaitOptions;
use ffx_writer::VerifiedMachineWriter;
use fho::{Error, FfxMain, FfxTool};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::time::Duration;

const DEFAULT_DIAGNOSTICS_TIMEOUT_SECS: f64 = 2.0;

#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    /// Successfully waited for the target (either to come up or shut down).
    Ok {},
}

#[cfg_attr(test, mockall::automock)]
pub trait DeviceWaiter {
    fn wait(
        &self,
        dur: Option<Duration>,
        env: &EnvironmentContext,
        target_spec: &Option<String>,
        behavior: ffx_target::WaitFor,
    ) -> impl Future<Output = Result<(), fho::Error>>;
}

pub struct DeviceWaiterImpl;

#[async_trait(?Send)]
impl fho::TryFromEnv for DeviceWaiterImpl {
    type Error = std::convert::Infallible;
    async fn try_from_env(_env: &fho::FhoEnvironment) -> Result<Self, Self::Error> {
        Ok(DeviceWaiterImpl)
    }
}

impl DeviceWaiter for DeviceWaiterImpl {
    async fn wait(
        &self,
        dur: Option<Duration>,
        env: &EnvironmentContext,
        target_spec: &Option<String>,
        behavior: ffx_target::WaitFor,
    ) -> Result<(), fho::Error> {
        ffx_target::wait_for_device(dur, env, target_spec, behavior).await
    }
}

use fho::FfxError;
use thiserror::Error;

#[derive(FfxError, Error, Debug)]
pub enum WaitError {
    #[exit_with_code(1)]
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[exit_with_code(1)]
    #[error("Config error: {0}")]
    Config(#[from] ffx_config::api::ConfigError),

    #[exit_with_code(1)]
    #[error("FFX Writer error: {0}")]
    Writer(#[from] ffx_writer::Error),

    #[exit_with_code(1)]
    #[error("Wait operation failed:\n{0}")]
    WaitFailed(String),

    #[exit_with_code(1)]
    #[error("Failed waiting for target to shut down: {0}")]
    WaitDownFailed(fho::Error),
}

#[derive(FfxTool)]
#[main_error(WaitError)]
pub struct WaitOperation<T: DeviceWaiter + fho::TryFromEnv> {
    #[command]
    pub cmd: WaitOptions,
    pub env: EnvironmentContext,
    pub waiter: T,
}

fho::embedded_plugin!(WaitOperation<DeviceWaiterImpl>, WaitError);

async fn get_diagnostics_string(env: &EnvironmentContext, timeout: u64, e: Error) -> String {
    let message = e.to_string();
    let timeout = if timeout > 0 {
        Duration::from_secs(timeout)
    } else {
        Duration::from_secs_f64(DEFAULT_DIAGNOSTICS_TIMEOUT_SECS)
    };
    let err = run_diagnostics(&env, timeout).await;
    format!("{message}\nDiagnostics:{err}")
}

async fn run_diagnostics(env: &EnvironmentContext, timeout: Duration) -> String {
    let mut notifier = ffx_diagnostics::StringNotifier::new();
    if let Err(e) = ffx_diagnostics_checks::run_diagnostics(&env, &mut notifier, timeout).await {
        notifier.on_error(format!("{e}")).unwrap();
    }
    notifier.into()
}

#[async_trait(?Send)]
impl<T: DeviceWaiter + fho::TryFromEnv> FfxMain for WaitOperation<T> {
    type Writer = VerifiedMachineWriter<CommandStatus>;
    type Error = WaitError;

    async fn main(self, mut writer: Self::Writer) -> Result<(), Self::Error> {
        match self.wait_impl().await {
            Ok(()) => {
                writer.machine(&CommandStatus::Ok {})?;
                Ok(())
            }
            Err(e) if self.cmd.down => {
                // If we are waiting for the device to go down, a failure means we cannot confirm it is down.
                // Running diagnostics makes no sense in this case.
                Err(WaitError::WaitDownFailed(e))
            }
            Err(e) => {
                let message = get_diagnostics_string(&self.env, self.cmd.timeout, e).await;
                Err(WaitError::WaitFailed(message))
            }
        }
    }
}

impl<T: DeviceWaiter + fho::TryFromEnv> WaitOperation<T> {
    pub async fn wait_impl(&self) -> Result<(), fho::Error> {
        let target_spec: Option<String> = ffx_target::get_target_specifier(&self.env)?;
        let behavior = if self.cmd.down {
            ffx_target::WaitFor::DeviceOffline
        } else {
            ffx_target::WaitFor::DeviceOnline
        };
        let duration =
            if self.cmd.timeout > 0 { Some(Duration::from_secs(self.cmd.timeout)) } else { None };
        self.waiter.wait(duration, &self.env, &target_spec, behavior).await
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use ffx_writer::{Format, TestBuffers};

    // This is just here to satisfy trait bounds.
    #[async_trait(?Send)]
    impl fho::TryFromEnv for MockDeviceWaiter {
        type Error = std::convert::Infallible;
        async fn try_from_env(_env: &fho::FhoEnvironment) -> Result<Self, Self::Error> {
            unimplemented!()
        }
    }

    #[fuchsia::test]
    async fn test_success() {
        let test_env = ffx_config::test_init().expect("test env");
        let mut mock_waiter = MockDeviceWaiter::new();
        mock_waiter.expect_wait().times(1).returning(|_, _, _, _| Box::pin(async { Ok(()) }));
        let tool = WaitOperation {
            cmd: WaitOptions { timeout: 1000, down: false },
            env: test_env.context.clone(),
            waiter: mock_waiter,
        };
        let test_buffers = TestBuffers::default();
        let writer = VerifiedMachineWriter::<CommandStatus>::new_test(
            Some(Format::JsonPretty),
            &test_buffers,
        );
        let res = tool.main(writer).await;
        let (stdout, stderr) = test_buffers.into_strings();
        assert!(res.is_ok(), "expected ok {stdout} {stderr}");
        let err = format!("schema not valid {stdout}");
        let json = serde_json::from_str(&stdout).expect(&err);
        let err = format!("json must adhere to schema: {json}");
        VerifiedMachineWriter::<CommandStatus>::verify_schema(&json).expect(&err)
    }

    #[fuchsia::test]
    async fn test_unexpected_error() {
        let test_env = ffx_config::test_init().expect("test env");
        let mut mock_waiter = MockDeviceWaiter::new();
        mock_waiter
            .expect_wait()
            .times(1)
            .returning(|_, _, _, _| Box::pin(async { Err(fho::bug!("oh no!")) }));
        let tool = WaitOperation {
            cmd: WaitOptions { timeout: 1000, down: false },
            env: test_env.context.clone(),
            waiter: mock_waiter,
        };
        let test_buffers = TestBuffers::default();
        let writer = VerifiedMachineWriter::<CommandStatus>::new_test(
            Some(Format::JsonPretty),
            &test_buffers,
        );
        let res = tool.main(writer).await;
        let (stdout, stderr) = test_buffers.into_strings();
        assert!(res.is_err(), "expected error {stdout} {stderr}");
        assert!(
            matches!(res, Err(WaitError::WaitFailed(_))),
            "expected 'WaitFailed' error {stdout} {stderr}"
        );
    }

    #[fuchsia::test]
    async fn test_user_error() {
        let test_env = ffx_config::test_init().expect("test env");
        let mut mock_waiter = MockDeviceWaiter::new();
        mock_waiter
            .expect_wait()
            .times(1)
            .returning(|_, _, _, _| Box::pin(async { Err(fho::user_error!("oh no!")) }));
        let tool = WaitOperation {
            cmd: WaitOptions { timeout: 1000, down: false },
            env: test_env.context.clone(),
            waiter: mock_waiter,
        };
        let test_buffers = TestBuffers::default();
        let writer = VerifiedMachineWriter::<CommandStatus>::new_test(
            Some(Format::JsonPretty),
            &test_buffers,
        );
        let res = tool.main(writer).await;
        let (stdout, stderr) = test_buffers.into_strings();
        assert!(res.is_err(), "expected error {stdout} {stderr}");
        assert!(
            matches!(res, Err(WaitError::WaitFailed(_))),
            "expected 'WaitFailed' error {stdout} {stderr}"
        );
    }

    #[fuchsia::test]
    async fn test_down_error_no_diagnostics() {
        let test_env = ffx_config::test_init().expect("test env");
        let mut mock_waiter = MockDeviceWaiter::new();
        mock_waiter
            .expect_wait()
            .times(1)
            .returning(|_, _, _, _| Box::pin(async { Err(fho::bug!("oh no!")) }));
        let tool = WaitOperation {
            cmd: WaitOptions { timeout: 1000, down: true },
            env: test_env.context.clone(),
            waiter: mock_waiter,
        };
        let test_buffers = TestBuffers::default();
        let writer = VerifiedMachineWriter::<CommandStatus>::new_test(
            Some(Format::JsonPretty),
            &test_buffers,
        );
        let res = tool.main(writer).await;
        let (stdout, stderr) = test_buffers.into_strings();
        assert!(res.is_err(), "expected error {stdout} {stderr}");
        if let Err(WaitError::WaitDownFailed(e)) = res {
            let err_msg = e.to_string();
            assert!(err_msg.contains("oh no!"), "expected 'oh no!' in error message: {err_msg}");
            assert!(
                !err_msg.contains("Diagnostics:"),
                "did not expect 'Diagnostics:' in error message: {err_msg}"
            );
        }
    }
}
