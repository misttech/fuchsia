// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use ffx_target_echo_args::EchoCommand;
use ffx_writer::VerifiedMachineWriter;
use fho::{FfxError, FfxMain, FfxTool};
use schemars::JsonSchema;
use serde::Serialize;
use target_connector::Connector;
use target_holders::fdomain::RemoteControlProxyHolder;
use thiserror::Error;

#[derive(FfxError, Error, Debug)]
pub enum EchoError {
    #[exit_with_code(1)]
    #[error("Failed to establish connection to Remote Control Service: {0}")]
    TargetConnectionFailed(#[source] fho::Error),

    #[exit_with_code(1)]
    #[error("Echo capability failed: {0}")]
    EchoCapabilityFailed(#[from] fidl::Error),

    #[exit_with_code(1)]
    #[error("FFX Writer error: {0}")]
    Writer(#[from] ffx_writer::Error),
}

#[derive(Debug, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EchoMessage {
    /// Message from the target
    Message(String),
    // Waiting on target
    Waiting(String),
    /// Unexpected error with string denoting error message.
    UnexpectedError(String),
}

#[derive(FfxTool)]
#[main_error(EchoError)]
pub struct EchoTool {
    #[command]
    cmd: EchoCommand,
    rcs_proxy: Connector<RemoteControlProxyHolder>,
}

fho::embedded_plugin!(EchoTool, EchoError);

#[async_trait(?Send)]
impl FfxMain for EchoTool {
    type Writer = VerifiedMachineWriter<EchoMessage>;
    type Error = EchoError;

    async fn main(self, mut writer: Self::Writer) -> Result<(), Self::Error> {
        match echo_impl(self.rcs_proxy, self.cmd, &mut writer).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let error_msg = e.to_string();
                let _ = writer.machine_or(&EchoMessage::UnexpectedError(error_msg), &e);
                Err(e)
            }
        }
    }
}

async fn echo_impl(
    rcs_proxy_connector: Connector<RemoteControlProxyHolder>,
    cmd: EchoCommand,
    writer: &mut VerifiedMachineWriter<EchoMessage>,
) -> Result<(), EchoError> {
    let echo_text = cmd.text.unwrap_or_else(|| "Ffx".to_string());
    // This outer loop retries connecting to the target every time the
    // connection fails. If we only connect once it only runs once.
    loop {
        // Get a connection to the target. If the target isn't there, this will
        // wait for it to appear. The closure is called just before that wait so
        // we can log what's happening before blocking a long time.
        //
        // If the daemon isn't available, we simply start it. If the daemon is
        // disabled from auto-starting with `daemon.autostart = false` then this
        // will still fail and exit the tool. Workflows that need tools to
        // auto-reconnect but still need to manually manage the daemon aren't
        // known to us at this time.
        //
        // Daemonless workflows should behave as though the daemon is always
        // reachable as far as this command is concerned, but daemonless is
        // experimental/unimplemented as of now so this isn't tested.
        let rcs_proxy = rcs_proxy_connector
            .try_connect(|target, connect_err| {
                let err_string = connect_err
                    .as_ref()
                    .map(|e| format!(". Error encountered: {e}"))
                    .unwrap_or_else(|| ".".to_owned());
                let message = if let Some(target) = &target {
                    format!("Waiting for target {target} to return{err_string}")
                } else {
                    format!("Waiting for target to return{err_string}")
                };
                writer
                    .machine_or(&EchoMessage::Waiting(message.clone()), message)
                    .map_err(|e| fho::Error::User(e.into()))?;
                Ok(())
            })
            .await
            .map_err(EchoError::TargetConnectionFailed)?;

        // This inner loop handles the repetition part of the --repeat argument.
        // If that argument wasn't specified then this too only runs once.
        loop {
            match rcs_proxy.echo_string(&echo_text).await {
                Ok(r) => {
                    let user_out = format!("SUCCESS: received {r:?}");
                    writer.machine_or(&EchoMessage::Message(r), user_out)?;
                }
                Err(e) => {
                    let message = format!("ERROR: {e:?}");
                    writer.machine_or(
                        &EchoMessage::UnexpectedError(message.clone()),
                        message.clone(),
                    )?;
                    if cmd.repeat {
                        break;
                    } else {
                        return Err(EchoError::EchoCapabilityFailed(e));
                    }
                }
            }

            if cmd.repeat {
                fuchsia_async::Timer::new(std::time::Duration::from_secs(1)).await;
            } else {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use anyhow::{Context, Result};
    use fdomain_fuchsia_developer_remotecontrol::{
        RemoteControlMarker, RemoteControlProxy, RemoteControlRequest,
    };
    use ffx_writer::{Format, TestBuffers};
    use fho::{FhoEnvironment, TryFromEnv};
    use futures::FutureExt;
    use serde_json::json;
    use std::sync::Arc;
    use target_behavior::ConnectionBehavior;
    use target_holders::FakeInjector;

    async fn setup_fake_service(client: Arc<fdomain_client::Client>) -> RemoteControlProxy {
        use futures::TryStreamExt;
        let (proxy, mut stream) = client.create_proxy_and_stream::<RemoteControlMarker>();
        fuchsia_async::Task::local(async move {
            while let Ok(Some(req)) = stream.try_next().await {
                match req {
                    RemoteControlRequest::EchoString { value, responder } => {
                        responder
                            .send(value.as_ref())
                            .context("error sending response")
                            .expect("should send");
                    }
                    _ => panic!("unexpected request: {:?}", req),
                }
            }
        })
        .detach();
        proxy
    }

    async fn setup_failing_fake_service(client: Arc<fdomain_client::Client>) -> RemoteControlProxy {
        use futures::TryStreamExt;
        let (proxy, mut stream) = client.create_proxy_and_stream::<RemoteControlMarker>();
        fuchsia_async::Task::local(async move {
            while let Ok(Some(req)) = stream.try_next().await {
                match req {
                    RemoteControlRequest::EchoString { value: _, responder } => {
                        // Intentionally drop the responder to trigger ClientChannelClosed
                        drop(responder);
                    }
                    _ => panic!("unexpected request: {:?}", req),
                }
            }
        })
        .detach();
        proxy
    }

    async fn run_echo_test(cmd: EchoCommand) -> Result<String> {
        let client = fdomain_local::local_client_empty();
        let fake_injector = Arc::new(FakeInjector {
            remote_factory_closure_f: Box::new(move || {
                Box::pin(setup_fake_service(Arc::clone(&client)).map(Ok))
            }),
            ..Default::default()
        });

        let env = FhoEnvironment::new_with_args(
            &ffx_config::EnvironmentContext::no_context(
                ffx_config::environment::ExecutableKind::Test,
                Default::default(),
                None,
                true,
            )?,
            &["some", "test"],
        );
        let target_env = target_behavior::target_interface(&env);
        target_env.set_behavior_for_test(ConnectionBehavior::DaemonConnector(fake_injector));

        let connector = Connector::try_from_env(&env).await.expect("Could not make test connector");
        let tool = EchoTool { cmd, rcs_proxy: connector };
        let buffers = TestBuffers::default();
        let writer = VerifiedMachineWriter::<EchoMessage>::new_test(None, &buffers);

        let result = tool.main(writer).await;
        assert!(result.is_ok());
        Ok(buffers.into_stdout_str())
    }

    #[fuchsia::test]
    async fn test_echo_with_no_text() -> Result<()> {
        let cmd = EchoCommand { text: None, repeat: false };
        let output = run_echo_test(cmd).await?;
        assert_eq!("SUCCESS: received \"Ffx\"\n".to_string(), output);
        Ok(())
    }

    #[fuchsia::test]
    async fn test_echo_with_text() -> Result<()> {
        let cmd = EchoCommand { text: Some("test".to_string()), repeat: false };
        let output = run_echo_test(cmd).await?;
        assert_eq!("SUCCESS: received \"test\"\n".to_string(), output);
        Ok(())
    }

    #[fuchsia::test]
    async fn test_echo_with_machine() -> Result<()> {
        let client = fdomain_local::local_client_empty();
        let fake_injector = Arc::new(FakeInjector {
            remote_factory_closure_f: Box::new(move || {
                Box::pin(setup_fake_service(Arc::clone(&client)).map(Ok))
            }),
            ..Default::default()
        });

        let env = FhoEnvironment::new_with_args(
            &ffx_config::EnvironmentContext::no_context(
                ffx_config::environment::ExecutableKind::Test,
                Default::default(),
                None,
                true,
            )?,
            &["some", "test"],
        );
        let target_env = target_behavior::target_interface(&env);
        target_env.set_behavior_for_test(ConnectionBehavior::DaemonConnector(fake_injector));
        let connector = Connector::try_from_env(&env).await.expect("Could not make test connector");
        let cmd = EchoCommand { text: Some("test".to_string()), repeat: false };
        let tool = EchoTool { cmd, rcs_proxy: connector };
        let buffers = TestBuffers::default();
        let writer = VerifiedMachineWriter::<EchoMessage>::new_test(Some(Format::Json), &buffers);

        let result = tool.main(writer).await;
        assert!(result.is_ok());

        let output = buffers.into_stdout_str();

        let err = format!("schema not valid {output}");
        let json = serde_json::from_str(&output).expect(&err);
        let err = format!("json must adhere to schema: {json}");
        VerifiedMachineWriter::<EchoMessage>::verify_schema(&json).expect(&err);

        let want = EchoMessage::Message("test".into());
        assert_eq!(json, json!(want));
        Ok(())
    }

    #[fuchsia::test]
    async fn test_echo_failure() -> Result<()> {
        let cmd = EchoCommand { text: None, repeat: false };
        let fake_injector = Arc::new(FakeInjector {
            remote_factory_closure_f: Box::new(move || {
                Box::pin(async { Err(anyhow::anyhow!("Mock connection failure")) })
            }),
            ..Default::default()
        });

        let env = FhoEnvironment::new_with_args(
            &ffx_config::EnvironmentContext::no_context(
                ffx_config::environment::ExecutableKind::Test,
                Default::default(),
                None,
                true,
            )?,
            &["some", "test"],
        );
        let target_env = target_behavior::target_interface(&env);
        target_env.set_behavior_for_test(ConnectionBehavior::DaemonConnector(fake_injector));

        let connector = Connector::try_from_env(&env).await.expect("Could not make test connector");
        let tool = EchoTool { cmd, rcs_proxy: connector };
        let buffers = TestBuffers::default();
        let writer = VerifiedMachineWriter::<EchoMessage>::new_test(None, &buffers);

        let result = tool.main(writer).await;

        match result {
            Err(EchoError::TargetConnectionFailed(_)) => {}
            _ => panic!("Expected Err(EchoError::TargetConnectionFailed), got {:?}", result),
        }
        Ok(())
    }

    #[fuchsia::test]
    async fn test_echo_capability_failure() -> Result<()> {
        let cmd = EchoCommand { text: Some("test".to_string()), repeat: false };
        let client = fdomain_local::local_client_empty();
        let fake_injector = Arc::new(FakeInjector {
            remote_factory_closure_f: Box::new(move || {
                Box::pin(setup_failing_fake_service(Arc::clone(&client)).map(Ok))
            }),
            ..Default::default()
        });

        let env = FhoEnvironment::new_with_args(
            &ffx_config::EnvironmentContext::no_context(
                ffx_config::environment::ExecutableKind::Test,
                Default::default(),
                None,
                true,
            )?,
            &["some", "test"],
        );
        let target_env = target_behavior::target_interface(&env);
        target_env.set_behavior_for_test(ConnectionBehavior::DaemonConnector(fake_injector));

        let connector = Connector::try_from_env(&env).await.expect("Could not make test connector");
        let tool = EchoTool { cmd, rcs_proxy: connector };
        let buffers = TestBuffers::default();
        let writer = VerifiedMachineWriter::<EchoMessage>::new_test(None, &buffers);

        let result = tool.main(writer).await;

        match result {
            Err(EchoError::EchoCapabilityFailed(_)) => {}
            _ => panic!("Expected Err(EchoError::EchoCapabilityFailed), got {:?}", result),
        }
        Ok(())
    }
}
