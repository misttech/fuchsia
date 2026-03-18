// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Result;
use async_trait::async_trait;
use fdomain_fuchsia_session_power::HandoffProxy;
use ffx_session_drop_power_lease_args::SessionDropPowerLeaseCommand;
use ffx_writer::SimpleWriter;
use fho::{FfxMain, FfxTool, user_error};
use target_holders::fdomain::moniker;

#[derive(FfxTool)]
pub struct DropPowerLeaseTool {
    #[command]
    cmd: SessionDropPowerLeaseCommand,
    #[with(moniker("/core/session-manager"))]
    handoff_proxy: HandoffProxy,
}

fho::embedded_plugin!(DropPowerLeaseTool);

#[async_trait(?Send)]
impl FfxMain for DropPowerLeaseTool {
    type Writer = SimpleWriter;
    async fn main(self, mut writer: Self::Writer) -> fho::Result<()> {
        drop_power_lease_impl(self.handoff_proxy, self.cmd, &mut writer).await?;
        Ok(())
    }
}

pub async fn drop_power_lease_impl<W: std::io::Write>(
    handoff_proxy: HandoffProxy,
    cmd: SessionDropPowerLeaseCommand,
    writer: &mut W,
) -> Result<()> {
    writeln!(writer, "Requesting to dropping power lease on execution state")?;
    let lease = match handoff_proxy.take().await? {
        Ok(lease) => Some(lease),
        Err(fdomain_fuchsia_session_power::HandoffError::AlreadyTaken) if cmd.allow_missing => {
            writeln!(writer, "Lease already dropped, ignoring error.")?;
            None
        }
        Err(err) => {
            return Err(
                user_error!("Failed to take power lease from session manager: {:?}", err).into()
            );
        }
    };
    writeln!(writer, "Success!")?;
    drop(lease);
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use fdomain_fuchsia_session_power::HandoffRequest;
    use target_holders::fdomain::fake_proxy;

    #[fuchsia::test]
    async fn test_drop_power_lease() {
        let client = fdomain_local::local_client_empty();
        let client_clone = std::sync::Arc::clone(&client);

        let proxy = fake_proxy(client, move |req| match req {
            HandoffRequest::Take { responder } => {
                let _ = responder.send(Ok(client_clone.create_event().into()));
            }
            x @ _ => unimplemented!("{x:?}"),
        });

        let drop_power_lease_cmd = SessionDropPowerLeaseCommand { allow_missing: false };
        let mut writer = Vec::new();
        let result = drop_power_lease_impl(proxy, drop_power_lease_cmd, &mut writer).await;
        assert!(result.is_ok());
        let output = String::from_utf8(writer).unwrap();
        assert_eq!(output, "Requesting to dropping power lease on execution state\nSuccess!\n");
    }

    #[fuchsia::test]
    async fn test_drop_power_lease_already_taken_error() {
        let client = fdomain_local::local_client_empty();
        let proxy = fake_proxy(std::sync::Arc::clone(&client), |req| match req {
            HandoffRequest::Take { responder } => {
                let _ =
                    responder.send(Err(fdomain_fuchsia_session_power::HandoffError::AlreadyTaken));
            }
            x @ _ => unimplemented!("{x:?}"),
        });

        let drop_power_lease_cmd = SessionDropPowerLeaseCommand { allow_missing: false };
        let mut writer = Vec::new();
        let result = drop_power_lease_impl(proxy, drop_power_lease_cmd, &mut writer).await;
        assert!(result.is_err());
    }

    #[fuchsia::test]
    async fn test_drop_power_lease_already_taken_allow_missing() {
        let client = fdomain_local::local_client_empty();
        let proxy = fake_proxy(std::sync::Arc::clone(&client), |req| match req {
            HandoffRequest::Take { responder } => {
                let _ =
                    responder.send(Err(fdomain_fuchsia_session_power::HandoffError::AlreadyTaken));
            }
            x @ _ => unimplemented!("{x:?}"),
        });

        let drop_power_lease_cmd = SessionDropPowerLeaseCommand { allow_missing: true };
        let mut writer = Vec::new();
        let result = drop_power_lease_impl(proxy, drop_power_lease_cmd, &mut writer).await;
        assert!(result.is_ok());
        let output = String::from_utf8(writer).unwrap();
        assert_eq!(
            output,
            "Requesting to dropping power lease on execution state\nLease already dropped, ignoring error.\nSuccess!\n"
        );
    }
}
