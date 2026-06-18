// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::ShowToolWrapper;
use crate::doctor_ledger::{
    DoctorLedger, LedgerMode, LedgerNode, LedgerNodeOp, LedgerOutcome, OutcomeFoldFunction,
};
use crate::single_target_diagnostics::run_single_target_diagnostics;
use anyhow::Result;
use ffx_config::EnvironmentContext;
use ffx_target::TargetInfoQuery;
use fidl::endpoints::create_proxy;
use fidl::prelude::*;
use fidl_fuchsia_developer_ffx::{
    DaemonProxy, TargetCollectionMarker, TargetCollectionProxy, TargetCollectionReaderMarker,
    TargetCollectionReaderRequest, TargetInfo, TargetMarker, TargetQuery, TargetState,
};
use fidl_fuchsia_developer_remotecontrol::{RemoteControlMarker, RemoteControlProxy};
use futures::TryStreamExt;
use std::io::Write;
use std::time::Duration;
use timeout::timeout;

pub async fn list_targets(
    query: Option<&str>,
    tc: &TargetCollectionProxy,
) -> Result<Vec<TargetInfo>> {
    let (reader, server) = fidl::endpoints::create_endpoints::<TargetCollectionReaderMarker>();

    tc.list_targets(
        &TargetQuery { string_matcher: query.map(|s| s.to_owned()), ..Default::default() },
        reader,
    )?;
    let mut res = Vec::new();
    let mut stream = server.into_stream();
    while let Ok(Some(TargetCollectionReaderRequest::Next { entry, responder })) =
        stream.try_next().await
    {
        responder.send()?;
        if !entry.is_empty() {
            res.extend(entry);
        } else {
            break;
        }
    }
    Ok(res)
}

pub fn target_name(target: &TargetInfo) -> String {
    target.nodename.clone().unwrap_or_else(|| ffx_target::UNKNOWN_TARGET_NAME.to_string())
}

pub fn make_ssh_fix_suggestion(ssh_log: &str) -> Option<&'static str> {
    if ssh_log.contains("Connection refused") {
        Some("SSH connection was refused. You may need to (re-)establish a tunnel connection.")
    } else if ssh_log.contains("Permission denied") {
        Some(
            "SSH connection could not authenticate. You may need to re-provision (pave or flash) your target to ensure SSH keys are appropriately setup.",
        )
    } else {
        None
    }
}

// Check a single target. Most steps can fail, which means we can't (or
// shouldn't) continue on to the later steps, so the pattern usually looks like:
//   let done = <step>;
//   if done { return }
pub async fn check_single_target_via_daemon<W: Write>(
    ledger: &mut DoctorLedger<W>,
    target: &TargetInfo,
    tc_proxy: &TargetCollectionProxy,
    show_tool: Option<&mut ShowToolWrapper>,
    retry_delay: Duration,
) -> Result<()> {
    let target_name = target_name(target);

    let done = check_product_state(ledger, target)?;
    if done {
        return Ok(());
    }
    let target_node = ledger.add_node(&format!("Target: {}", target_name), LedgerMode::Normal)?;

    check_compatibility(ledger, target)?;

    let (target_proxy, done) =
        get_target_proxy(ledger, target, tc_proxy, retry_delay, target_node).await?;
    if done {
        return Ok(());
    }

    let (remote_proxy, done) =
        get_remote_proxy_via_daemon(ledger, retry_delay, target_node, target_proxy).await?;
    if done {
        return Ok(());
    }

    let done = check_identify_host(ledger, retry_delay, target_node, remote_proxy).await?;
    if done {
        return Ok(());
    }

    show_target(ledger, target, show_tool).await?;

    ledger.close(target_node)?;
    Ok(())
}

// Check a single target. Most steps can fail, which means we can't (or
// shouldn't) continue on to the later steps, so the pattern usually looks like:
//   let done = <step>;
//   if done { return }
pub async fn check_single_target_locally<W: Write>(
    ledger: &mut DoctorLedger<W>,
    target: &TargetInfo,
    env_context: &EnvironmentContext,
    show_tool: Option<&mut ShowToolWrapper>,
    retry_delay: Duration,
) -> Result<()> {
    let done = check_product_state(ledger, target)?;
    if done {
        return Ok(());
    }

    // We don't check compatibility in direct-mode, because we are using FDomain, which
    // doesn't use the ssh-connection as a mechanism for establishing compatibility.
    // Instead it relies on FIDL versioning.

    // run_target-diagnostics() validates RCS, IdentifyHost, etc, so we don't need other checks
    let node = ledger.add_node(
        &format!("Running diagnostics against {}", target_name(target)),
        LedgerMode::Verbose,
    )?;
    run_target_diagnostics(ledger, target, env_context, retry_delay).await?;
    ledger.close(node)?;

    show_target(ledger, target, show_tool).await?;

    Ok(())
}

// TODO(b/423023263): This function is missing test coverage. There should either be
// something mockable here, and the underlying crate should also be tested.
pub async fn run_target_diagnostics<W: Write>(
    ledger: &mut DoctorLedger<W>,
    target: &TargetInfo,
    env_context: &EnvironmentContext,
    retry_delay: Duration,
) -> Result<(), anyhow::Error> {
    match run_single_target_diagnostics(env_context, target.clone(), ledger, retry_delay).await {
        Ok(()) => Ok(()),
        Err(e) => {
            let node = ledger.add_node(
                &format!("Error encountered in diagnostics: {e}"),
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            Ok(())
        }
    }
}

pub async fn show_target<W: Write>(
    ledger: &mut DoctorLedger<W>,
    target: &TargetInfo,
    show_tool: Option<&mut ShowToolWrapper>,
) -> Result<(), anyhow::Error> {
    Ok(if let Some(show_tool) = show_tool {
        let node =
            ledger.add_node("Running `ffx target show` against device", LedgerMode::Automatic)?;
        ledger.set_outcome(node, LedgerOutcome::Info)?;
        match show_tool.allocate(target.nodename.clone()).await {
            Ok(_) => {
                let node = ledger.add(LedgerNode::new(
                    "Allocating proxies for `target show`".to_string(),
                    LedgerMode::Verbose,
                ))?;
                ledger.set_outcome(node, LedgerOutcome::Success)?;
                match show_tool.run().await {
                    Ok((stdout, stderr)) => {
                        let node = ledger.add(LedgerNode::new(
                            "Executing `ffx target show`".to_string(),
                            LedgerMode::Verbose,
                        ))?;
                        ledger.set_outcome(node, LedgerOutcome::Success)?;
                        let node = ledger.add(LedgerNode::new(
                            format!("stdout:\n\t{}", stdout.replace("\n", "\n\t"),),
                            LedgerMode::Verbose,
                        ))?;
                        ledger.set_outcome(node, LedgerOutcome::Info)?;
                        if !stderr.is_empty() {
                            let node = ledger.add(LedgerNode::new(
                                format!("stderr:\n\t{}", stderr.replace("\n", "\n\t")),
                                LedgerMode::Verbose,
                            ))?;
                            ledger.set_outcome(node, LedgerOutcome::Info)?;
                        }
                    }
                    Err(e) => {
                        let node = ledger.add_node(
                            &format!("Error executing `target show`: {:?}", e),
                            LedgerMode::Verbose,
                        )?;
                        ledger.set_outcome(node, LedgerOutcome::Failure)?;
                    }
                }
            }
            Err(e) => {
                let node = ledger.add_node(
                    &format!("Error while setting up `target show`: {:?}", e),
                    LedgerMode::Normal,
                )?;
                ledger.set_outcome(node, LedgerOutcome::Failure)?;
            }
        };
    })
}

pub async fn check_identify_host<W: Write>(
    ledger: &mut DoctorLedger<W>,
    retry_delay: Duration,
    target_node: usize,
    remote_proxy: RemoteControlProxy,
) -> Result<bool, anyhow::Error> {
    Ok(match timeout(retry_delay, remote_proxy.identify_host()).await {
        Ok(Ok(_)) => {
            let node = ledger
                .add(LedgerNode::new("Communicating with RCS".to_string(), LedgerMode::Verbose))?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;
            false
        }
        Ok(Err(e)) => {
            let node = ledger.add_node(
                &format!("Error while communicating with RCS: {}", e),
                LedgerMode::Verbose,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(target_node)?;
            true
        }
        Err(_) => {
            let node =
                ledger.add_node("Timeout while communicating with RCS", LedgerMode::Verbose)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(target_node)?;
            true
        }
    })
}

pub fn check_product_state<W: Write>(
    ledger: &mut DoctorLedger<W>,
    target: &TargetInfo,
) -> Result<bool, anyhow::Error> {
    Ok(match target.target_state {
        None => false,
        Some(TargetState::Unknown | TargetState::Disconnected | TargetState::Product) => false,
        Some(TargetState::Fastboot) => {
            let node = ledger.add_node(
                &format!(
                    "Target found in fastboot mode: {}",
                    target.serial_number.as_deref().unwrap_or("UNKNOWN serial number")
                ),
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;
            true
        }
        Some(TargetState::Zedboot) => {
            let node = ledger.add_node(
                &format!("Skipping target in zedboot: {}", target_name(target)),
                LedgerMode::Automatic,
            )?;
            ledger.set_outcome(node, LedgerOutcome::SoftWarning)?;
            true
        }
    })
}

pub async fn get_remote_proxy_via_daemon<W: Write>(
    ledger: &mut DoctorLedger<W>,
    retry_delay: Duration,
    target_node: usize,
    target_proxy: ffx_target::TargetProxy,
) -> Result<(RemoteControlProxy, bool), anyhow::Error> {
    let (remote_proxy, remote_server_end) = create_proxy::<RemoteControlMarker>();
    let done = match timeout(retry_delay, target_proxy.open_remote_control(remote_server_end)).await
    {
        Ok(Ok(res)) => {
            let node = ledger.add_node("Connecting to RCS", LedgerMode::Verbose)?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;
            match res {
                Ok(_) => false,
                Err(_) => {
                    let logs = match target_proxy.get_ssh_logs().await {
                        Ok(l) => l,
                        Err(e) => {
                            let _ = ledger.close(target_node);
                            return Err(e.into());
                        }
                    };
                    let node = ledger.add_node(
                        &format!("Error while connecting to RCS: could not establish SSH connection to the target: {}", logs),
                        LedgerMode::Verbose,
                    )?;
                    ledger.set_outcome(node, LedgerOutcome::Failure)?;
                    if let Some(suggestion) = make_ssh_fix_suggestion(&logs) {
                        let node = ledger.add_node(suggestion, LedgerMode::Automatic)?;
                        ledger.set_outcome(node, LedgerOutcome::Info)?;
                    }
                    ledger.close(target_node)?;
                    true
                }
            }
        }
        Ok(Err(e)) => {
            let node = ledger
                .add_node(&format!("Error while connecting to RCS: {}", e), LedgerMode::Verbose)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(target_node)?;
            true
        }
        Err(_) => {
            let node = ledger.add_node("Timeout while connecting to RCS", LedgerMode::Verbose)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(target_node)?;
            true
        }
    };
    Ok((remote_proxy, done))
}

pub async fn get_target_proxy<W: Write>(
    ledger: &mut DoctorLedger<W>,
    target: &TargetInfo,
    tc_proxy: &TargetCollectionProxy,
    retry_delay: Duration,
    target_node: usize,
) -> Result<(ffx_target::TargetProxy, bool), anyhow::Error> {
    let (target_proxy, target_server) = fidl::endpoints::create_proxy::<TargetMarker>();
    let done = match timeout(
        retry_delay,
        tc_proxy.open_target(
            &TargetQuery { string_matcher: target.nodename.clone(), ..Default::default() },
            target_server,
        ),
    )
    .await
    {
        Ok(Ok(_)) => {
            let node = ledger.add_node("Opened target handle", LedgerMode::Verbose)?;
            ledger.set_outcome(node, LedgerOutcome::Success)?;
            false
        }
        Ok(Err(e)) => {
            let node = ledger.add_node(
                &format!("Error while opening target handle: {}", e),
                LedgerMode::Verbose,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(target_node)?;
            true
        }
        Err(_) => {
            let node =
                ledger.add_node("Timeout while opening target handle", LedgerMode::Verbose)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(target_node)?;
            true
        }
    };
    Ok((target_proxy, done))
}

pub fn check_compatibility<W: Write>(
    ledger: &mut DoctorLedger<W>,
    target: &TargetInfo,
) -> Result<(), anyhow::Error> {
    let (compatibility_state, compatibility_message) = match &target.compatibility {
        Some(info) => (info.state.into(), info.message.clone()),
        None => (
            compat_info::CompatibilityState::Absent,
            "Compatibility information is not available".to_string(),
        ),
    };
    let state_node = ledger
        .add_node(&format!("Compatibility state: {compatibility_state}"), LedgerMode::Verbose)?;
    let message_node = ledger.add_node(&compatibility_message, LedgerMode::Verbose)?;
    let outcome = match compatibility_state {
        compat_info::CompatibilityState::Supported => LedgerOutcome::Success,
        compat_info::CompatibilityState::Error => LedgerOutcome::Failure,
        compat_info::CompatibilityState::Absent => LedgerOutcome::SoftWarning,
        compat_info::CompatibilityState::Unsupported => LedgerOutcome::Warning,
        compat_info::CompatibilityState::Unknown => LedgerOutcome::SoftWarning,
    };
    ledger.set_outcome(state_node, outcome)?;
    ledger.set_outcome(message_node, outcome)?;
    Ok(())
}

pub async fn check_targets_locally<W: Write>(
    ledger: &mut DoctorLedger<W>,
    target_str: &str,
    env_context: &EnvironmentContext,
    mut show_tool: Option<ShowToolWrapper>,
    retry_delay: Duration,
) -> Result<()> {
    let query = TargetInfoQuery::try_from(target_str)?;
    let discovery_node = ledger.add_node("Searching for targets", LedgerMode::Automatic)?;
    let find_res = find_targets_locally(env_context, query).await;
    let targets = check_target_discovery(ledger, find_res)?;
    ledger.close(discovery_node)?;
    if targets.is_empty() {
        return Ok(());
    }
    let mut verify_inode = LedgerNode::new("Verifying Targets".to_string(), LedgerMode::Normal);
    verify_inode.set_fold_function(OutcomeFoldFunction::FailureToSuccess, LedgerOutcome::Failure);
    let main_node = ledger.add(verify_inode)?;
    for target in targets.iter() {
        let target_node =
            ledger.add_node(&format!("Target: {}", target_name(target)), LedgerMode::Normal)?;
        check_single_target_locally(ledger, target, env_context, show_tool.as_mut(), retry_delay)
            .await?;
        ledger.close(target_node)?;
    }
    ledger.close(main_node)?;
    Ok(())
}

pub fn check_target_discovery<W: Write>(
    ledger: &mut DoctorLedger<W>,
    targets_result: Result<Vec<TargetInfo>>,
) -> Result<Vec<TargetInfo>> {
    Ok(match targets_result {
        Ok(targets) => {
            if !targets.is_empty() {
                let node = ledger
                    .add_node(&format!("{} targets found", targets.len()), LedgerMode::Automatic)?;
                ledger.set_outcome(node, LedgerOutcome::Success)?;
                targets
            } else {
                let node = ledger.add_node("No targets found!", LedgerMode::Automatic)?;
                ledger.set_outcome(node, LedgerOutcome::Failure)?;
                vec![]
            }
        }
        Err(e) => {
            let node =
                ledger.add_node(&format!("Error getting targets: {e}"), LedgerMode::Normal)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            vec![]
        }
    })
}

pub async fn find_targets_locally(
    env_context: &EnvironmentContext,
    query: TargetInfoQuery,
) -> Result<Vec<TargetInfo>> {
    let targets = ffx_target::get_discovered_targets(query, true, true, env_context).await?;
    Ok(targets.into_iter().map(|t| TargetInfo::from(t)).collect::<Vec<TargetInfo>>())
}

pub async fn check_targets_via_daemon<W: Write>(
    ledger: &mut DoctorLedger<W>,
    target_str: &str,
    retry_delay: Duration,
    env_context: &EnvironmentContext,
    mut show_tool: Option<ShowToolWrapper>,
    run_additional_diagnostics: bool,
    daemon_proxy: &DaemonProxy,
) -> Result<(), anyhow::Error> {
    let (tc_proxy, tc_server) = fidl::endpoints::create_proxy::<TargetCollectionMarker>();
    let discovery_node = ledger.add_node("Searching for targets", LedgerMode::Automatic)?;
    match timeout(
        retry_delay,
        daemon_proxy
            .connect_to_protocol(TargetCollectionMarker::PROTOCOL_NAME, tc_server.into_channel()),
    )
    .await
    {
        Ok(Err(e)) => {
            let node = ledger.add_node(
                &format!("Error connecting to target service: {}", e),
                LedgerMode::Verbose,
            )?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(discovery_node)?;
            return Ok(());
        }
        Ok(_) => {}
        Err(_) => {
            let node = ledger
                .add_node("Timeout while connecting to target service", LedgerMode::Verbose)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(discovery_node)?;
            return Ok(());
        }
    }
    let targets = match timeout(retry_delay, list_targets(Some(target_str), &tc_proxy)).await {
        Ok(targets_result) => {
            let targets = check_target_discovery(ledger, targets_result)?;
            if targets.is_empty() {
                ledger.close(discovery_node)?;
                return Ok(());
            }
            targets
        }
        Err(_) => {
            let node =
                ledger.add_node("Timeout while getting target list", LedgerMode::Automatic)?;
            ledger.set_outcome(node, LedgerOutcome::Failure)?;
            ledger.close(discovery_node)?;
            return Ok(());
        }
    };
    ledger.close(discovery_node)?;
    let mut verify_inode = LedgerNode::new("Verifying Targets".to_string(), LedgerMode::Normal);
    verify_inode.set_fold_function(OutcomeFoldFunction::FailureToSuccess, LedgerOutcome::Failure);
    let main_node = ledger.add(verify_inode)?;
    for target in targets.iter() {
        match check_single_target_via_daemon(
            ledger,
            target,
            &tc_proxy,
            show_tool.as_mut(),
            retry_delay,
        )
        .await
        {
            Ok(_) => {}
            Err(e) => {
                // This might be formatted strangely, as it's covering edge cases around things
                // like being unable to get info from a target FIDL proxy, but generally if we're
                // able to connect to RCS the above function will succeed.
                let node = ledger.add_node(
                    format!("Error checking target: {e}").as_str(),
                    LedgerMode::Automatic,
                )?;
                ledger.set_outcome(node, LedgerOutcome::Failure)?;
            }
        }

        if run_additional_diagnostics {
            let target_name = target_name(target);
            let node = ledger.add_node(
                &format!("Running additional diagnostics against {target_name}"),
                LedgerMode::Automatic,
            )?;

            // This function is only intended to return an error if it is surfaced from the ledger
            // itself, so don't worry about it prematurely breaking out of the loop.
            run_target_diagnostics(ledger, target, env_context, retry_delay).await?;
            ledger.close(node)?;
        }
    }
    ledger.close(main_node)?;
    Ok(())
}
