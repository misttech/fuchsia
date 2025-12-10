// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use analytics::add_custom_event;
use anyhow::Result;
use async_trait::async_trait;
use errors::{ffx_bail, ffx_bail_with_code};
use ffx_config::EnvironmentContext;
use ffx_list_args::ListCommand;
use ffx_target::{TargetInfo, TargetInfoQuery};
use ffx_writer::{ToolIO as _, VerifiedMachineWriter};
use fho::{Deferred, FfxMain, FfxTool, deferred};
use fidl_fuchsia_developer_ffx::{self as ffx};
use futures::TryStreamExt;
use target_behavior::{ConnectionBehavior, target_interface};
use target_formatter::{JsonTarget, JsonTargetFormatter, TargetFormatter};
use target_holders::daemon_protocol;

#[derive(FfxTool)]
#[target(None)]
pub struct ListTool {
    #[command]
    cmd: ListCommand,
    #[with(deferred(daemon_protocol()))]
    tc_proxy: Deferred<ffx::TargetCollectionProxy>,
    context: EnvironmentContext,
    fho_env: fho::FhoEnvironment,
}

fho::embedded_plugin!(ListTool);

#[async_trait(?Send)]
impl FfxMain for ListTool {
    type Writer = VerifiedMachineWriter<Vec<JsonTarget>>;
    async fn main(mut self, mut writer: Self::Writer) -> fho::Result<()> {
        self.update_from_target();
        // XXX Shouldn't check `is_strict()`. Eventually we'll _always_ do local discovery,
        // at which point this check goes away.
        let direct_mode = self.context.is_strict()
            || self.context.get_direct_connection_mode()
            || !ffx_target::is_discovery_enabled(&self.context).await;

        let spec = ffx_target::get_target_specifier(&self.context)?;
        let query = TargetInfoQuery::from(spec.clone());
        let infos = if direct_mode {
            self.list_targets_direct(query).await?
        } else {
            let fidl_infos = list_targets(self.tc_proxy.await?, &self.cmd).await?;
            fidl_infos
                .into_iter()
                .map(|fi| {
                    let mut ti = TargetInfo::from(fi);
                    if !matches!(query, TargetInfoQuery::First) && ti.match_query(&query) {
                        ti.is_default = Some(true);
                    }
                    ti
                })
                .collect()
        };
        emit_device_stats_event(infos.len(), &spec).await;
        show_targets(self.cmd, infos, &mut writer).await?;
        Ok(())
    }
}

impl ListTool {
    // Users might reasonable expect that they can say `ffx -t foo target list`, rather
    // than `ffx target list foo`. Update the environment as though they had typed the
    // command "correctly". (If they use both, the positional argument at the end takes
    // precedence over the "-t" argument.)
    fn update_from_target(&mut self) {
        if self.cmd.nodename.is_some() {
            self.context.override_target_specifier(&self.cmd.nodename);
        }
    }

    async fn list_targets_direct(&self, query: TargetInfoQuery) -> Result<Vec<TargetInfo>> {
        let connect_to_rcs =
            !self.cmd.no_probe && !matches!(self.cmd.format, ffx_list_args::Format::Addresses);
        Ok(match query.get_target_addr() {
            Some(addr) if connect_to_rcs => {
                // We don't need to do discovery, and in fact may not be able to
                // discover the device. So instead, just query the information
                // directly.  We're going to assume this device is in product mode.
                // (Note: we check whether we explicitly told _not_ to connect to RCS, in
                // which case we're not going to get anything useful from trying to do an IdentifyHost.
                // If the device is undiscoverable _and_ we cannot connect to RCS,
                // then there's not much to be done. Unfortunately we can't
                // know if a device is undiscoverable or not, so we can't give the
                // user useful guidance in that situation.)
                let target_env = target_interface(&self.fho_env);
                let behavior = target_env.init_connection_behavior(&self.context).await?;
                let ConnectionBehavior::DirectConnector(ref connector) = *behavior else {
                    ffx_bail!("Could not get direct connector for {}", String::from(query));
                };
                vec![connector.resolution().await?.get_target_info(addr, &self.context).await?]
            }
            _ => {
                ffx_target::list_targets(
                    &self.context,
                    query.clone(),
                    !self.cmd.no_usb,
                    !self.cmd.no_mdns,
                    connect_to_rcs,
                )
                .await?
            }
        })
    }
}

async fn show_targets(
    cmd: ListCommand,
    mut infos: Vec<TargetInfo>,
    writer: &mut VerifiedMachineWriter<Vec<JsonTarget>>,
) -> Result<()> {
    // Provide stable output. Use "unstable" since we don't care about the original ordering.
    infos.sort_unstable_by(|a, b| a.nodename.cmp(&b.nodename));
    match infos.len() {
        0 => {
            // Printed to stderr, so that if a user is parsing output, say from a formatted
            // output, that the message is not consumed. A stronger future strategy would
            // have richer behavior dependent upon whether the user has a controlling
            // terminal, which would require passing in more and richer IO delegates.
            if let Some(n) = cmd.nodename {
                ffx_bail_with_code!(2, "Device {} not found.", n);
            } else {
                if !writer.is_machine() {
                    writeln!(writer.stderr(), "No devices found.")?;
                } else {
                    writer.machine(&Vec::new())?;
                }
            }
        }
        _ => {
            let address_types = cmd.address_types();
            if address_types.is_empty() {
                ffx_bail!("Invalid arguments, you must allow at least one address type")
            }
            if writer.is_machine() {
                let res = target_formatter::filter_targets_by_address_types(infos, address_types);
                let formatter = JsonTargetFormatter::try_from(res)?;
                writer.machine(&formatter.targets)?;
            } else {
                let formatter =
                    Box::<dyn TargetFormatter>::try_from((cmd.format, address_types, infos))?;
                writer.line(formatter.lines().join("\n"))?;
            }
        }
    }
    Ok(())
}

async fn list_targets(
    tc_proxy: ffx::TargetCollectionProxy,
    cmd: &ListCommand,
) -> Result<Vec<ffx::TargetInfo>> {
    let (reader, server) = fidl::endpoints::create_endpoints::<ffx::TargetCollectionReaderMarker>();

    tc_proxy.list_targets(
        &ffx::TargetQuery { string_matcher: cmd.nodename.clone(), ..Default::default() },
        reader,
    )?;
    let mut res = Vec::new();
    let mut stream = server.into_stream();
    while let Ok(Some(ffx::TargetCollectionReaderRequest::Next { entry, responder })) =
        stream.try_next().await
    {
        responder.send()?;
        if entry.len() > 0 {
            res.extend(entry);
        } else {
            break;
        }
    }

    Ok(res)
}

fn query_type(query: &str) -> &str {
    match query.into() {
        TargetInfoQuery::NodenameOrSerial(_) => "nodename_or_serial",
        TargetInfoQuery::Serial(_) => "serial",
        TargetInfoQuery::Addr(_) => "addr",
        TargetInfoQuery::VSock(_) => "vsock",
        TargetInfoQuery::Usb(_) => "usb",
        TargetInfoQuery::First => "first",
    }
}

/// Emit an event indicating how many devices were in the result.
pub async fn emit_device_stats_event(num_devices: usize, query: &Option<String>) {
    let query = query.as_ref().map_or("", |v| v);
    let _ = add_custom_event(
        Some("ffx_target_list_devices"),
        Some(query_type(query)),
        None,
        [("devices", (num_devices as u64).into())].into_iter().collect(),
    )
    .await;
} ///////////////////////////////////////////////////////////////////////////////
// tests

#[cfg(test)]
mod test {
    use super::*;
    use addr::TargetAddr;
    use ffx_command::FfxCommandLine;
    use ffx_list_args::{AddressTypes, Format};
    use ffx_target::info::{RemoteControlState, TargetState};
    use ffx_writer::TestBuffers;
    use fidl_fuchsia_developer_ffx as ffx;
    use fidl_fuchsia_developer_ffx::TargetInfo as FidlTargetInfo;
    use regex::Regex;
    use std::net::IpAddr;
    use target_behavior::DirectConnector;
    use target_holders::fake_proxy;

    fn tab_list_cmd(nodename: Option<String>) -> ListCommand {
        ListCommand { nodename, format: Format::Tabular, ..Default::default() }
    }

    fn to_fidl_target(nodename: String, vsock: bool) -> FidlTargetInfo {
        let addr: TargetAddr = if vsock {
            TargetAddr::VSockCtx(42)
        } else {
            TargetAddr::new(
                IpAddr::from([0xfe80, 0x0, 0x0, 0x0, 0xdead, 0xbeef, 0xbeef, 0xbeef]),
                3,
                0,
            )
        };
        FidlTargetInfo {
            nodename: Some(nodename),
            addresses: Some(vec![addr.into()]),
            age_ms: Some(101),
            rcs_state: Some(ffx::RemoteControlState::Up),
            target_state: Some(ffx::TargetState::Unknown),
            ..Default::default()
        }
    }

    fn setup_fake_target_collection_server(
        num_tests: usize,
        vsock: bool,
    ) -> ffx::TargetCollectionProxy {
        fake_proxy(move |req| match req {
            ffx::TargetCollectionRequest::ListTargets { query, reader, .. } => {
                let reader = reader.into_proxy();
                let fidl_values: Vec<FidlTargetInfo> =
                    if query.string_matcher.as_deref().map(|s| s.is_empty()).unwrap_or(true) {
                        (0..num_tests)
                            .map(|i| format!("Test {}", i))
                            .map(|name| to_fidl_target(name, vsock))
                            .collect()
                    } else {
                        let v = query.string_matcher.unwrap();
                        (0..num_tests)
                            .map(|i| format!("Test {}", i))
                            .filter(|t| *t == v)
                            .map(|name| to_fidl_target(name, vsock))
                            .collect()
                    };
                fuchsia_async::Task::local(async move {
                    let mut iter = fidl_values.chunks(10);
                    loop {
                        let chunk = iter.next().unwrap_or(&[]);
                        reader.next(&chunk).await.unwrap();
                        if chunk.is_empty() {
                            break;
                        }
                    }
                })
                .detach();
            }
            r => panic!("unexpected request: {:?}", r),
        })
    }

    async fn try_run_list_test(num_tests: usize, cmd: ListCommand, vsock: bool) -> Result<String> {
        let proxy = setup_fake_target_collection_server(num_tests, vsock);
        let test_buffers = TestBuffers::default();
        let mut writer = VerifiedMachineWriter::new_test(None, &test_buffers);
        let infos = list_targets(proxy, &cmd).await?.into_iter().map(|ti| ti.into()).collect();
        show_targets(cmd, infos, &mut writer).await?;
        Ok(test_buffers.into_stdout_str())
    }

    async fn run_list_test(num_tests: usize, cmd: ListCommand, vsock: bool) -> String {
        try_run_list_test(num_tests, cmd, vsock).await.unwrap()
    }

    #[fuchsia::test]
    async fn test_machine_schema() {
        let proxy = setup_fake_target_collection_server(3, false);
        let test_buffers = TestBuffers::default();
        let mut writer =
            VerifiedMachineWriter::new_test(Some(ffx_writer::Format::Json), &test_buffers);
        let cmd = ListCommand { format: Format::Tabular, ..Default::default() };
        let infos = list_targets(proxy, &cmd)
            .await
            .expect("list targets")
            .into_iter()
            .map(|ti| ti.into())
            .collect();
        show_targets(cmd, infos, &mut writer).await.expect("show_targets");
        let data_str = test_buffers.into_stdout_str();
        let data = serde_json::from_str(&data_str).expect("json value");
        match VerifiedMachineWriter::<Vec<JsonTarget>>::verify_schema(&data) {
            Ok(_) => (),
            Err(e) => {
                panic!("error verifying schema of {data:?}: {e}");
            }
        };
    }

    #[fuchsia::test]
    async fn test_machine_schema_vsock() {
        let proxy = setup_fake_target_collection_server(3, true);
        let test_buffers = TestBuffers::default();
        let mut writer =
            VerifiedMachineWriter::new_test(Some(ffx_writer::Format::Json), &test_buffers);
        let cmd = ListCommand { format: Format::Tabular, ..Default::default() };
        let infos = list_targets(proxy, &cmd)
            .await
            .expect("list targets")
            .into_iter()
            .map(|ti| ti.into())
            .collect();
        show_targets(cmd, infos, &mut writer).await.expect("show_targets");
        let data_str = test_buffers.into_stdout_str();
        let data = serde_json::from_str(&data_str).expect("json value");
        match VerifiedMachineWriter::<Vec<JsonTarget>>::verify_schema(&data) {
            Ok(_) => (),
            Err(e) => {
                panic!("error verifying schema of {data:?}: {e}");
            }
        };
    }

    #[fuchsia::test]
    async fn test_list_with_no_devices_and_no_nodename() -> Result<()> {
        let output = run_list_test(0, tab_list_cmd(None), false).await;
        assert_eq!("".to_string(), output);
        let output = run_list_test(0, tab_list_cmd(None), true).await;
        assert_eq!("".to_string(), output);
        Ok(())
    }

    #[fuchsia::test]
    async fn test_list_with_one_device_and_no_nodename() -> Result<()> {
        let output = run_list_test(1, tab_list_cmd(None), false).await;
        let value = format!("Test {}", 0);
        let node_listing = Regex::new(&value).expect("test regex");
        assert_eq!(
            1,
            node_listing.find_iter(&output).count(),
            "could not find \"{}\" nodename in output:\n{}",
            value,
            output
        );
        Ok(())
    }

    #[fuchsia::test]
    async fn test_list_with_one_device_and_no_nodename_vsock() -> Result<()> {
        let output = run_list_test(1, tab_list_cmd(None), true).await;
        let value = format!("Test {}", 0);
        let node_listing = Regex::new(&value).expect("test regex");
        assert_eq!(
            1,
            node_listing.find_iter(&output).count(),
            "could not find \"{}\" nodename in output:\n{}",
            value,
            output
        );
        Ok(())
    }

    #[fuchsia::test]
    async fn test_list_with_multiple_devices_and_no_nodename() -> Result<()> {
        let num_tests = 10;
        let output = run_list_test(num_tests, tab_list_cmd(None), false).await;
        for x in 0..num_tests {
            let value = format!("Test {}", x);
            let node_listing = Regex::new(&value).expect("test regex");
            assert_eq!(
                1,
                node_listing.find_iter(&output).count(),
                "could not find \"{}\" nodename in output:\n{}",
                value,
                output
            );
        }
        Ok(())
    }

    #[fuchsia::test]
    async fn test_list_with_one_device_and_matching_nodename() -> Result<()> {
        let output = run_list_test(1, tab_list_cmd(Some("Test 0".to_string())), false).await;
        let value = format!("Test {}", 0);
        let node_listing = Regex::new(&value).expect("test regex");
        assert_eq!(
            1,
            node_listing.find_iter(&output).count(),
            "could not find \"{}\" nodename in output:\n{}",
            value,
            output
        );
        Ok(())
    }

    #[fuchsia::test]
    async fn test_list_with_one_device_and_not_matching_nodename() -> Result<()> {
        let output = try_run_list_test(1, tab_list_cmd(Some("blarg".to_string())), false).await;
        assert!(output.is_err());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_list_with_multiple_devices_and_not_matching_nodename() -> Result<()> {
        let num_tests = 25;
        let output =
            try_run_list_test(num_tests, tab_list_cmd(Some("blarg".to_string())), false).await;
        assert!(output.is_err());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_list_with_multiple_devices_and_matching_nodename() -> Result<()> {
        let output = run_list_test(25, tab_list_cmd(Some("Test 19".to_string())), false).await;
        let value = format!("Test {}", 0);
        let node_listing = Regex::new(&value).expect("test regex");
        assert_eq!(0, node_listing.find_iter(&output).count());
        let value = format!("Test {}", 19);
        let node_listing = Regex::new(&value).expect("test regex");
        assert_eq!(1, node_listing.find_iter(&output).count());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_list_with_address_types_none() -> Result<()> {
        let num_tests = 25;
        let cmd_none = ListCommand {
            no_ipv4: true,
            no_ipv6: true,
            allow_addrs: AddressTypes::IP,
            ..Default::default()
        };
        let output = try_run_list_test(num_tests, cmd_none, false).await;
        assert!(output.is_err());
        Ok(())
    }

    #[test]
    fn test_address_types_from_cmd() -> Result<()> {
        let cmd_none = ListCommand { no_ipv4: true, no_ipv6: true, ..Default::default() };
        assert_eq!(cmd_none.address_types(), AddressTypes::IP.complement());
        let cmd_ipv4_only = ListCommand { no_ipv4: false, no_ipv6: true, ..Default::default() };
        assert_eq!(cmd_ipv4_only.address_types(), AddressTypes::IPV6.complement());
        let cmd_ipv6_only = ListCommand { no_ipv4: true, no_ipv6: false, ..Default::default() };
        assert_eq!(cmd_ipv6_only.address_types(), AddressTypes::IPV4.complement());
        let cmd_all = ListCommand { no_ipv4: false, no_ipv6: false, ..Default::default() };
        assert_eq!(cmd_all.address_types(), AddressTypes::all());
        let cmd_all_default = ListCommand::default();
        assert_eq!(cmd_all_default.address_types(), AddressTypes::all());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_sorted_output() -> Result<()> {
        let cmd = ListCommand::default();
        let test_buffers = TestBuffers::default();
        let mut writer = VerifiedMachineWriter::new_test(None, &test_buffers);
        let ti1 = TargetInfo {
            nodename: Some(String::from("z")),
            addresses: vec![],
            rcs_state: RemoteControlState::Unknown,
            target_state: TargetState::Unknown,
            ..Default::default()
        };
        let ti2 = TargetInfo { nodename: Some(String::from("a")), ..ti1.clone() };
        let infos = vec![ti1, ti2];
        show_targets(cmd, infos, &mut writer).await?;
        let out: Vec<String> =
            test_buffers.into_stdout_str().lines().map(|s| s.to_string()).collect();
        // Line 0 is the header
        assert!(out[1].starts_with("a"));
        assert!(out[2].starts_with("z"));
        Ok(())
    }

    async fn build_list_tool(
        cmd: ListCommand,
        env: &ffx_config::TestEnv,
        fho_env: fho::FhoEnvironment,
    ) -> ListTool {
        ListTool {
            cmd,
            tc_proxy: fho::TryFromEnvWith::try_from_env_with(deferred(daemon_protocol()), &fho_env)
                .await
                .expect("deferred tc_proxy failed"),
            fho_env,
            context: env.context.clone(),
        }
    }

    #[fuchsia::test]
    async fn test_update_from_target_overrides_context() {
        let ffx_cmd_line = FfxCommandLine::default();
        let env = ffx_config::test_init().unwrap();
        let fho_env = fho::FhoEnvironment::new(&env.context, &ffx_cmd_line);
        let list_cmd =
            ListCommand { nodename: Some(String::from("mytarget")), ..Default::default() };
        let mut tool = build_list_tool(list_cmd, &env, fho_env).await;
        tool.update_from_target();

        let spec = ffx_target::get_target_specifier(&tool.context).unwrap();
        assert_eq!(spec, Some(String::from("mytarget")));
    }

    #[fuchsia::test]
    async fn test_update_from_target_no_op_when_none() {
        let ffx_cmd_line = FfxCommandLine::default();
        let env = ffx_config::test_init().unwrap();
        let fho_env = fho::FhoEnvironment::new(&env.context, &ffx_cmd_line);
        let list_cmd = ListCommand { nodename: None, ..Default::default() };
        let mut tool = build_list_tool(list_cmd, &env, fho_env).await;

        // Capture initial state
        let initial_spec = ffx_target::get_target_specifier(&tool.context).unwrap();

        tool.update_from_target();

        // Post-condition: spec should be unchanged
        assert_eq!(ffx_target::get_target_specifier(&tool.context).unwrap(), initial_spec);
    }

    #[fuchsia::test]
    async fn test_list_direct_uses_resolution() {
        let env = ffx_config::test_init().unwrap();
        let ffx_cmd_line = FfxCommandLine::default();
        let fho_env = fho::FhoEnvironment::new(&env.context, &ffx_cmd_line);

        let resolution =
            ffx_target::Resolution::mock(|| Err(anyhow::anyhow!("MockConnectionError")));
        let behavior = ConnectionBehavior::DirectConnector(
            DirectConnector::from_resolution_for_test(resolution),
        );

        let target_env = target_interface(&fho_env);
        target_env.set_behavior_for_test(behavior);

        let list_cmd = ListCommand::default();
        let tool = build_list_tool(list_cmd, &env, fho_env).await;

        let query = TargetInfoQuery::Addr("127.0.0.1:8022".parse().unwrap());

        let res = tool.list_targets_direct(query).await;

        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("MockConnectionError"));
    }
}
