// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use discovery::{FastbootTargetState, TargetHandle, TargetState};
use errors::{ffx_bail, ffx_error};
use ffx_config::EnvironmentContext;
use ffx_reboot_args::RebootCommand;
use ffx_writer::SimpleWriter;
use fho::{Deferred, FfxContext, FfxMain, FfxTool};
use fidl_fuchsia_developer_ffx::{TargetRebootError, TargetRebootState};
use fidl_fuchsia_hardware_power_statecontrol::{
    AdminProxy, ShutdownAction, ShutdownOptions, ShutdownReason,
};
use target_holders::{TargetProxyHolder, moniker};
use tokio::sync::mpsc::channel;

const NETSVC_NOT_FOUND: &str = "The Fuchsia target's netsvc address could not be determined.\n\
                                If this problem persists, try running `ffx doctor` for diagnostics";
const NETSVC_COMM_ERR: &str = "There was a communication error using netsvc to reboot.\n\
                               If the problem persists, try running `ffx doctor` for further diagnostics";
const BOOT_TO_ZED: &str = "Cannot reboot from Bootloader state to Recovery state.";
const REBOOT_TO_PRODUCT: &str = "\nReboot to Product state with `ffx target reboot` and try again.";
const COMM_ERR: &str = "There was a communication error with the device. Please try again. \n\
                        If the problem persists, try running `ffx doctor` for further diagnostics";

#[derive(FfxTool)]
pub struct RebootTool {
    context: EnvironmentContext,
    #[command]
    cmd: RebootCommand,
    // Use target proxy when in daemon mode
    target_proxy: Deferred<TargetProxyHolder>,
    // Use admin proxy when in direct mode
    #[with(fho::deferred(moniker("/bootstrap/shutdown_shim")))]
    admin_proxy: Deferred<AdminProxy>,
}

fho::embedded_plugin!(RebootTool);

#[async_trait(?Send)]
impl FfxMain for RebootTool {
    type Writer = SimpleWriter;
    async fn main(mut self, _writer: Self::Writer) -> fho::Result<()> {
        if self.context.get_direct_connection_mode() {
            reboot_direct(&mut self.admin_proxy, self.cmd, &self.context).await
        } else {
            reboot_daemon(&self.target_proxy.await?, self.cmd).await
        }
    }
}

async fn reboot_direct(
    admin_proxy: &mut Deferred<AdminProxy>,
    cmd: RebootCommand,
    context: &EnvironmentContext,
) -> Result<(), fho::Error> {
    let state = reboot_state(&cmd)?;
    // Discover the device, because we may need to reach it directly if it's in fastboot mode
    let handle = ffx_target::discover_single_default_target(context).await?;
    reboot_direct_with_handle(handle, admin_proxy, state, context).await
}

async fn reboot_direct_with_handle(
    handle: TargetHandle,
    admin_proxy: &mut Deferred<AdminProxy>,
    state: TargetRebootState,
    context: &EnvironmentContext,
) -> Result<(), fho::Error> {
    match handle.state {
        TargetState::Product { .. } => reboot_direct_from_product(admin_proxy.await?, state).await,
        TargetState::Fastboot(fastboot_state) => {
            reboot_direct_from_fastboot(handle.node_name, fastboot_state, context, state).await
        }
        s => ffx_bail!("Rebooting a target in state {s} is not supported in direct mode"),
    }
}

async fn reboot_direct_from_product(
    admin_proxy: AdminProxy,
    state: TargetRebootState,
) -> Result<(), fho::Error> {
    let action = match state {
        TargetRebootState::Product => ShutdownAction::Reboot,
        TargetRebootState::Bootloader => ShutdownAction::RebootToBootloader,
        TargetRebootState::Recovery => ShutdownAction::RebootToRecovery,
    };
    let options = ShutdownOptions {
        action: Some(action),
        reasons: Some(vec![ShutdownReason::DeveloperRequest]),
        ..Default::default()
    };
    // There are two errors: the outer error, which represents a FIDL failure, and the inner error
    // which is the Shutdown() failure.  The daemon version ignores the shutdown failure, so so will
    // we.
    let _res = match admin_proxy.shutdown(&options).await {
        e @ Err(fidl::Error::ClientChannelClosed { protocol_name, .. }) => {
            // If the 'protocol_name' is 'fuchsia.hardware.power.statecontrol.Admin'
            // then we can be more confident that target reboot/shutdown has succeeded.
            if protocol_name == "fuchsia.hardware.power.statecontrol.Admin" {
                log::info!("Target reboot succeeded.");
            } else {
                log::info!(
                    "Assuming target reboot succeeded. Client received a PEER_CLOSED from '{protocol_name}'"
                );
            }
            log::debug!("{e:?}");
            return Ok(());
        }
        Err(e) => return Err(e).bug_context("Shutting down target"),
        Ok(res) => res,
    };
    Ok(())
}

async fn reboot_direct_from_fastboot(
    node_name: Option<String>,
    fastboot_state: FastbootTargetState,
    context: &EnvironmentContext,
    target_state: TargetRebootState,
) -> Result<(), fho::Error> {
    // TODO(473553526): refactor this and the equivalent code in
    // daemon/protocols/target_collection/src/reboot.rs
    let mut fastboot_interface = ffx_fastboot_connection_factory::get_fastboot_interface(
        &fastboot_state,
        node_name,
        context,
    )
    .await?;
    match target_state {
        TargetRebootState::Product => {
            fastboot_interface.reboot().await.map_err(|e| ffx_error!("Cannot reboot: {e:?}"))?
        }
        TargetRebootState::Bootloader => {
            let (reboot_client, mut reboot_server) = channel(1);
            let reboot_fut = fastboot_interface.reboot_bootloader(reboot_client);
            let drain_fut = async { while reboot_server.recv().await.is_some() {} };
            let (res, _) = futures::join!(reboot_fut, drain_fut);
            res.map_err(|e| ffx_error!("Cannot reboot to bootloader: {e:?}"))?
        }
        TargetRebootState::Recovery => ffx_bail!("Cannot reboot from fastboot to recovery"),
    }
    Ok(())
}

async fn reboot_daemon(target_proxy: &TargetProxyHolder, cmd: RebootCommand) -> fho::Result<()> {
    let state = reboot_state(&cmd)?;
    match target_proxy.reboot(state).await.bug()? {
        Ok(_) => Ok(()),
        Err(TargetRebootError::NetsvcCommunication) => {
            ffx_bail!("{}", NETSVC_COMM_ERR)
        }
        Err(TargetRebootError::NetsvcAddressNotFound) => {
            ffx_bail!("{}", NETSVC_NOT_FOUND)
        }
        Err(TargetRebootError::FastbootToRecovery) => {
            ffx_bail!("{}{}", BOOT_TO_ZED, REBOOT_TO_PRODUCT)
        }
        Err(TargetRebootError::TargetCommunication)
        | Err(TargetRebootError::FastbootCommunication) => ffx_bail!("{}", COMM_ERR),
    }
}

fn reboot_state(cmd: &RebootCommand) -> fho::Result<TargetRebootState> {
    match (cmd.bootloader, cmd.recovery) {
        (true, true) => {
            ffx_bail!("Cannot specify booth bootloader and recovery switches at the same time.")
        }
        (true, false) => Ok(TargetRebootState::Bootloader),
        (false, true) => Ok(TargetRebootState::Recovery),
        (false, false) => Ok(TargetRebootState::Product),
    }
}

////////////////////////////////////////////////////////////////////////////////
// tests
#[cfg(test)]
mod test {
    use super::*;
    use fidl_fuchsia_developer_ffx::{TargetProxy, TargetRequest};
    use target_holders::fake_proxy;

    fn setup_fake_target_server(cmd: RebootCommand) -> TargetProxyHolder {
        TargetProxyHolder::from(fake_proxy::<TargetProxy>(move |req| match req {
            TargetRequest::Reboot { state: _, responder } => {
                assert!(!(cmd.bootloader && cmd.recovery));
                responder.send(Ok(())).unwrap();
            }
            r => panic!("unexpected request: {:?}", r),
        }))
    }

    async fn run_reboot_daemon_test(cmd: RebootCommand) -> fho::Result<()> {
        let target_proxy = setup_fake_target_server(cmd);
        reboot_daemon(&target_proxy, cmd).await
    }

    #[fuchsia::test]
    async fn test_reboot() -> fho::Result<()> {
        run_reboot_daemon_test(RebootCommand { bootloader: false, recovery: false }).await
    }

    #[fuchsia::test]
    async fn test_bootloader() -> fho::Result<()> {
        run_reboot_daemon_test(RebootCommand { bootloader: true, recovery: false }).await
    }

    #[fuchsia::test]
    async fn test_recovery() -> fho::Result<()> {
        run_reboot_daemon_test(RebootCommand { bootloader: false, recovery: true }).await
    }

    #[fuchsia::test]
    async fn test_error() {
        assert!(
            run_reboot_daemon_test(RebootCommand { bootloader: true, recovery: true })
                .await
                .is_err()
        )
    }

    #[fuchsia::test]
    async fn test_reboot_direct_from_product() -> fho::Result<()> {
        let admin_proxy = fake_proxy::<AdminProxy>(|req| match req {
            fidl_fuchsia_hardware_power_statecontrol::AdminRequest::Shutdown {
                options,
                responder,
            } => {
                assert_eq!(options.action, Some(ShutdownAction::Reboot));
                responder.send(Ok(())).unwrap();
            }
            r => panic!("unexpected request: {:?}", r),
        });
        reboot_direct_from_product(admin_proxy, TargetRebootState::Product).await
    }

    #[fuchsia::test]
    async fn test_reboot_direct_from_product_bootloader() -> fho::Result<()> {
        let admin_proxy = fake_proxy::<AdminProxy>(|req| match req {
            fidl_fuchsia_hardware_power_statecontrol::AdminRequest::Shutdown {
                options,
                responder,
            } => {
                assert_eq!(options.action, Some(ShutdownAction::RebootToBootloader));
                responder.send(Ok(())).unwrap();
            }
            r => panic!("unexpected request: {:?}", r),
        });
        reboot_direct_from_product(admin_proxy, TargetRebootState::Bootloader).await
    }

    #[fuchsia::test]
    async fn test_reboot_direct_from_product_recovery() -> fho::Result<()> {
        let admin_proxy = fake_proxy::<AdminProxy>(|req| match req {
            fidl_fuchsia_hardware_power_statecontrol::AdminRequest::Shutdown {
                options,
                responder,
            } => {
                assert_eq!(options.action, Some(ShutdownAction::RebootToRecovery));
                responder.send(Ok(())).unwrap();
            }
            r => panic!("unexpected request: {:?}", r),
        });
        reboot_direct_from_product(admin_proxy, TargetRebootState::Recovery).await
    }

    #[fuchsia::test]
    async fn test_reboot_direct_zedboot_error() {
        let env = ffx_config::test_init().unwrap();
        let handle = TargetHandle {
            node_name: Some("foo".to_string()),
            state: TargetState::Zedboot,
            manual: false,
        };
        // We can pass a dummy admin proxy since it won't be used
        let mut admin_proxy = Deferred::from_output(Ok(fake_proxy::<AdminProxy>(|_req| {
            panic!("unexpected request")
        })));

        let res = reboot_direct_with_handle(
            handle,
            &mut admin_proxy,
            TargetRebootState::Product,
            &env.context,
        )
        .await;

        assert!(res.is_err());
        assert!(
            res.unwrap_err()
                .to_string()
                .contains("Rebooting a target in state Zedboot is not supported")
        );
    }
}
