// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::ShowToolWrapper;
use crate::doctor_ledger::{LedgerMode, LedgerNode, LedgerNodeGuard, LedgerOutcome};
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

pub async fn check_single_target_via_daemon<W: Write>(
    ledger: &mut LedgerNodeGuard<'_, W>,
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
    let mut target_node =
        ledger.add_node(&format!("Target: {}", target_name), LedgerMode::Normal)?;

    check_compatibility(&mut target_node, target)?;

    let (target_proxy, done) =
        get_target_proxy(&mut target_node, target, tc_proxy, retry_delay).await?;
    if done {
        return Ok(());
    }

    let (remote_proxy, done) =
        get_remote_proxy_via_daemon(&mut target_node, retry_delay, target_proxy).await?;
    if done {
        return Ok(());
    }

    let done = check_identify_host(&mut target_node, retry_delay, remote_proxy).await?;
    if done {
        return Ok(());
    }

    show_target(&mut target_node, target, show_tool).await?;

    Ok(())
}

pub async fn check_single_target_locally<W: Write>(
    ledger: &mut LedgerNodeGuard<'_, W>,
    target: &TargetInfo,
    env_context: &EnvironmentContext,
    show_tool: Option<&mut ShowToolWrapper>,
    retry_delay: Duration,
) -> Result<()> {
    let done = check_product_state(ledger, target)?;
    if done {
        return Ok(());
    }

    {
        let mut node = ledger.add_node(
            &format!("Running diagnostics against {}", target_name(target)),
            LedgerMode::Verbose,
        )?;
        run_target_diagnostics(&mut node, target, env_context, retry_delay).await?;
    }

    show_target(ledger, target, show_tool).await?;

    Ok(())
}

pub async fn run_target_diagnostics<W: Write>(
    ledger: &mut LedgerNodeGuard<'_, W>,
    target: &TargetInfo,
    env_context: &EnvironmentContext,
    retry_delay: Duration,
) -> Result<(), anyhow::Error> {
    match run_single_target_diagnostics(env_context, target.clone(), ledger, retry_delay).await {
        Ok(()) => Ok(()),
        Err(e) => {
            ledger
                .add_node(&format!("Error encountered in diagnostics: {e}"), LedgerMode::Automatic)?
                .set_outcome(LedgerOutcome::Failure)?;
            Ok(())
        }
    }
}

pub async fn show_target<W: Write>(
    ledger: &mut LedgerNodeGuard<'_, W>,
    target: &TargetInfo,
    show_tool: Option<&mut ShowToolWrapper>,
) -> Result<(), anyhow::Error> {
    Ok(if let Some(show_tool) = show_tool {
        let mut node =
            ledger.add_node("Running `ffx target show` against device", LedgerMode::Automatic)?;
        match show_tool.allocate(target.nodename.clone()).await {
            Ok(_) => {
                node.add(LedgerNode::new(
                    "Allocating proxies for `target show`".to_string(),
                    LedgerMode::Verbose,
                ))?
                .set_outcome(LedgerOutcome::Success)?;
                match show_tool.run().await {
                    Ok((stdout, stderr)) => {
                        node.add(LedgerNode::new(
                            "Executing `ffx target show`".to_string(),
                            LedgerMode::Verbose,
                        ))?
                        .set_outcome(LedgerOutcome::Success)?;
                        node.add(LedgerNode::new(
                            format!("stdout:\n\t{}", stdout.replace("\n", "\n\t"),),
                            LedgerMode::Verbose,
                        ))?
                        .set_outcome(LedgerOutcome::Info)?;
                        if !stderr.is_empty() {
                            node.add(LedgerNode::new(
                                format!("stderr:\n\t{}", stderr.replace("\n", "\n\t")),
                                LedgerMode::Verbose,
                            ))?
                            .set_outcome(LedgerOutcome::Info)?;
                        }
                    }
                    Err(e) => {
                        node.add_node(
                            &format!("Error executing `target show`: {:?}", e),
                            LedgerMode::Verbose,
                        )?
                        .set_outcome(LedgerOutcome::Failure)?;
                    }
                }
            }
            Err(e) => {
                node.add_node(
                    &format!("Error while setting up `target show`: {:?}", e),
                    LedgerMode::Normal,
                )?
                .set_outcome(LedgerOutcome::Failure)?;
            }
        };
        node.set_outcome(LedgerOutcome::Info)?;
    })
}

pub async fn check_identify_host<W: Write>(
    ledger: &mut LedgerNodeGuard<'_, W>,
    retry_delay: Duration,
    remote_proxy: RemoteControlProxy,
) -> Result<bool, anyhow::Error> {
    Ok(match timeout(retry_delay, remote_proxy.identify_host()).await {
        Ok(Ok(_)) => {
            ledger
                .add(LedgerNode::new("Communicating with RCS".to_string(), LedgerMode::Verbose))?
                .set_outcome(LedgerOutcome::Success)?;
            false
        }
        Ok(Err(e)) => {
            ledger
                .add_node(
                    &format!("Error while communicating with RCS: {}", e),
                    LedgerMode::Verbose,
                )?
                .set_outcome(LedgerOutcome::Failure)?;
            true
        }
        Err(_) => {
            ledger
                .add_node("Timeout while communicating with RCS", LedgerMode::Verbose)?
                .set_outcome(LedgerOutcome::Failure)?;
            true
        }
    })
}

pub fn check_product_state<W: Write>(
    ledger: &mut LedgerNodeGuard<'_, W>,
    target: &TargetInfo,
) -> Result<bool, anyhow::Error> {
    Ok(match target.target_state {
        None => false,
        Some(TargetState::Unknown | TargetState::Disconnected | TargetState::Product) => false,
        Some(TargetState::Fastboot) => {
            ledger
                .add_node(
                    &format!(
                        "Target found in fastboot mode: {}",
                        target.serial_number.as_deref().unwrap_or("UNKNOWN serial number")
                    ),
                    LedgerMode::Automatic,
                )?
                .set_outcome(LedgerOutcome::Success)?;
            true
        }
        Some(TargetState::Zedboot) => {
            ledger
                .add_node(
                    &format!("Skipping target in zedboot: {}", target_name(target)),
                    LedgerMode::Automatic,
                )?
                .set_outcome(LedgerOutcome::SoftWarning)?;
            true
        }
    })
}

pub async fn get_remote_proxy_via_daemon<W: Write>(
    ledger: &mut LedgerNodeGuard<'_, W>,
    retry_delay: Duration,
    target_proxy: ffx_target::TargetProxy,
) -> Result<(RemoteControlProxy, bool), anyhow::Error> {
    let (remote_proxy, remote_server_end) = create_proxy::<RemoteControlMarker>();
    let done = match timeout(retry_delay, target_proxy.open_remote_control(remote_server_end)).await
    {
        Ok(Ok(res)) => {
            ledger
                .add_node("Connecting to RCS", LedgerMode::Verbose)?
                .set_outcome(LedgerOutcome::Success)?;
            match res {
                Ok(_) => false,
                Err(_) => {
                    let logs = match target_proxy.get_ssh_logs().await {
                        Ok(l) => l,
                        Err(e) => {
                            return Err(e.into());
                        }
                    };
                    ledger.add_node(
                        &format!("Error while connecting to RCS: could not establish SSH connection to the target: {}", logs),
                        LedgerMode::Verbose,
                    )?.set_outcome(LedgerOutcome::Failure)?;
                    if let Some(suggestion) = make_ssh_fix_suggestion(&logs) {
                        ledger
                            .add_node(suggestion, LedgerMode::Automatic)?
                            .set_outcome(LedgerOutcome::Info)?;
                    }
                    true
                }
            }
        }
        Ok(Err(e)) => {
            ledger
                .add_node(&format!("Error while connecting to RCS: {}", e), LedgerMode::Verbose)?
                .set_outcome(LedgerOutcome::Failure)?;
            true
        }
        Err(_) => {
            ledger
                .add_node("Timeout while connecting to RCS", LedgerMode::Verbose)?
                .set_outcome(LedgerOutcome::Failure)?;
            true
        }
    };
    Ok((remote_proxy, done))
}

pub async fn get_target_proxy<W: Write>(
    ledger: &mut LedgerNodeGuard<'_, W>,
    target: &TargetInfo,
    tc_proxy: &TargetCollectionProxy,
    retry_delay: Duration,
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
            ledger
                .add_node("Opened target handle", LedgerMode::Verbose)?
                .set_outcome(LedgerOutcome::Success)?;
            false
        }
        Ok(Err(e)) => {
            ledger
                .add_node(
                    &format!("Error while opening target handle: {}", e),
                    LedgerMode::Verbose,
                )?
                .set_outcome(LedgerOutcome::Failure)?;
            true
        }
        Err(_) => {
            ledger
                .add_node("Timeout while opening target handle", LedgerMode::Verbose)?
                .set_outcome(LedgerOutcome::Failure)?;
            true
        }
    };
    Ok((target_proxy, done))
}

pub fn check_compatibility<W: Write>(
    ledger: &mut LedgerNodeGuard<'_, W>,
    target: &TargetInfo,
) -> Result<(), anyhow::Error> {
    let (compatibility_state, compatibility_message) = match &target.compatibility {
        Some(info) => (info.state.into(), info.message.clone()),
        None => (
            compat_info::CompatibilityState::Absent,
            "Compatibility information is not available".to_string(),
        ),
    };
    let outcome = match compatibility_state {
        compat_info::CompatibilityState::Supported => LedgerOutcome::Success,
        compat_info::CompatibilityState::Error => LedgerOutcome::Failure,
        compat_info::CompatibilityState::Absent => LedgerOutcome::SoftWarning,
        compat_info::CompatibilityState::Unsupported => LedgerOutcome::Warning,
        compat_info::CompatibilityState::Unknown => LedgerOutcome::SoftWarning,
    };
    let mut state_node = ledger
        .add_node(&format!("Compatibility state: {compatibility_state}"), LedgerMode::Verbose)?;
    state_node.add_node(&compatibility_message, LedgerMode::Verbose)?.set_outcome(outcome)?;
    state_node.set_outcome(outcome)?;
    Ok(())
}

pub async fn check_targets_locally<W: Write>(
    ledger: &mut LedgerNodeGuard<'_, W>,
    target_str: &str,
    env_context: &EnvironmentContext,
    mut show_tool: Option<ShowToolWrapper>,
    retry_delay: Duration,
) -> Result<()> {
    let query = TargetInfoQuery::try_from(target_str)?;
    let targets = {
        let mut discovery_node = ledger.add_node("Searching for targets", LedgerMode::Automatic)?;
        let find_res = find_targets_locally(env_context, query).await;
        check_target_discovery(&mut discovery_node, find_res)?
    };
    if targets.is_empty() {
        return Ok(());
    }
    for target in targets.iter() {
        let mut target_node =
            ledger.add_node(&format!("Target: {}", target_name(target)), LedgerMode::Normal)?;
        check_single_target_locally(
            &mut target_node,
            target,
            env_context,
            show_tool.as_mut(),
            retry_delay,
        )
        .await?;
    }
    Ok(())
}

pub fn check_target_discovery<W: Write>(
    ledger: &mut LedgerNodeGuard<'_, W>,
    targets_result: Result<Vec<TargetInfo>>,
) -> Result<Vec<TargetInfo>> {
    Ok(match targets_result {
        Ok(targets) => {
            if !targets.is_empty() {
                ledger
                    .add_node(&format!("{} targets found", targets.len()), LedgerMode::Automatic)?
                    .set_outcome(LedgerOutcome::Success)?;
                targets
            } else {
                ledger
                    .add_node("No targets found!", LedgerMode::Automatic)?
                    .set_outcome(LedgerOutcome::Failure)?;
                vec![]
            }
        }
        Err(e) => {
            ledger
                .add_node(&format!("Error getting targets: {e}"), LedgerMode::Normal)?
                .set_outcome(LedgerOutcome::Failure)?;
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
    ledger: &mut LedgerNodeGuard<'_, W>,
    target_str: &str,
    retry_delay: Duration,
    env_context: &EnvironmentContext,
    mut show_tool: Option<ShowToolWrapper>,
    run_additional_diagnostics: bool,
    daemon_proxy: &DaemonProxy,
) -> Result<(), anyhow::Error> {
    let (tc_proxy, tc_server) = fidl::endpoints::create_proxy::<TargetCollectionMarker>();
    let targets = {
        let mut discovery_node = ledger.add_node("Searching for targets", LedgerMode::Automatic)?;
        match timeout(
            retry_delay,
            daemon_proxy.connect_to_protocol(
                TargetCollectionMarker::PROTOCOL_NAME,
                tc_server.into_channel(),
            ),
        )
        .await
        {
            Ok(Err(e)) => {
                discovery_node
                    .add_node(
                        &format!("Error connecting to target service: {}", e),
                        LedgerMode::Verbose,
                    )?
                    .set_outcome(LedgerOutcome::Failure)?;
                return Ok(());
            }
            Ok(_) => {}
            Err(_) => {
                discovery_node
                    .add_node("Timeout while connecting to target service", LedgerMode::Verbose)?
                    .set_outcome(LedgerOutcome::Failure)?;
                return Ok(());
            }
        }
        let targets_res = timeout(retry_delay, list_targets(Some(target_str), &tc_proxy)).await;
        match targets_res {
            Ok(targets_result) => {
                let targets = check_target_discovery(&mut discovery_node, targets_result)?;
                if targets.is_empty() {
                    return Ok(());
                }
                targets
            }
            Err(_) => {
                discovery_node
                    .add_node("Timeout while getting target list", LedgerMode::Automatic)?
                    .set_outcome(LedgerOutcome::Failure)?;
                return Ok(());
            }
        }
    };
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
                ledger
                    .add_node(
                        format!("Error checking target: {e}").as_str(),
                        LedgerMode::Automatic,
                    )?
                    .set_outcome(LedgerOutcome::Failure)?;
            }
        }

        if run_additional_diagnostics {
            let target_name = target_name(target);
            let mut node = ledger.add_node(
                &format!("Running additional diagnostics against {target_name}"),
                LedgerMode::Automatic,
            )?;
            run_target_diagnostics(&mut node, target, env_context, retry_delay).await?;
        }
    }
    Ok(())
}
