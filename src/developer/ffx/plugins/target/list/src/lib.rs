// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use analytics::add_custom_event;
use anyhow::Result;
use async_trait::async_trait;
use errors::{ffx_bail, ffx_bail_with_code};
use ffx_config::EnvironmentContext;
use ffx_list_args::{AddressTypes, ListCommand};
use ffx_target::{KnockError, TargetInfoQuery};
use ffx_writer::{ToolIO as _, VerifiedMachineWriter};
use fho::{deferred, Deferred, FfxMain, FfxTool};
use fidl_fuchsia_developer_ffx as ffx;
use fuchsia_async::TimeoutExt;
use futures::{StreamExt, TryStreamExt};
use std::time::Duration;
use target_formatter::{JsonTarget, JsonTargetFormatter, TargetFormatter};
use target_holders::daemon_protocol;

fn address_types_from_cmd(cmd: &ListCommand) -> AddressTypes {
    if cmd.no_ipv4 && cmd.no_ipv6 {
        AddressTypes::None
    } else if cmd.no_ipv4 {
        AddressTypes::Ipv6Only
    } else if cmd.no_ipv6 {
        AddressTypes::Ipv4Only
    } else {
        AddressTypes::All
    }
}

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
    async fn main(self, mut writer: Self::Writer) -> fho::Result<()> {
        let cmd = self.update_from_target();
        // XXX Shouldn't check `is_strict()`. Eventually we'll _always_ do local discovery,
        // at which point this check goes away.
        let infos = if self.context.is_strict()
            || self.context.get_direct_connection_mode()
            || !ffx_target::is_discovery_enabled(&self.context).await
        {
            local_list_targets(&self.context, &cmd).await?
        } else {
            list_targets(self.tc_proxy.await?, &cmd).await?
        };
        emit_device_stats_event(infos.len(), &cmd.nodename).await;
        show_targets(cmd, infos, &mut writer, &self.context).await?;
        Ok(())
    }
}

impl ListTool {
    // Users might reasonable expect that they can say `ffx -t foo target list`, rather
    // than `ffx target list foo`. Update the ListCommand as though they had typed the
    // command "correctly". (If they use both, the positional argument at the end takes
    // precedence over the "-t" argument.)
    fn update_from_target(&self) -> ListCommand {
        let cmd = self.cmd.clone();
        if cmd.nodename.is_some() {
            cmd
        } else {
            ListCommand { nodename: self.fho_env.ffx_command().global.target.clone(), ..cmd }
        }
    }
}

async fn show_targets(
    cmd: ListCommand,
    mut infos: Vec<ffx::TargetInfo>,
    writer: &mut VerifiedMachineWriter<Vec<JsonTarget>>,
    context: &EnvironmentContext,
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
            let address_types = address_types_from_cmd(&cmd);
            if let AddressTypes::None = address_types {
                ffx_bail!("Invalid arguments, cannot specify both --no_ipv4 and --no_ipv6")
            }
            if writer.is_machine() {
                let res = target_formatter::filter_targets_by_address_types(infos, address_types);
                let mut formatter = JsonTargetFormatter::try_from(res)?;
                let default: Option<String> = ffx_target::get_target_specifier(&context).await?;
                JsonTargetFormatter::set_default_target(&mut formatter.targets, default.as_deref());
                writer.machine(&formatter.targets)?;
            } else {
                let formatter =
                    Box::<dyn TargetFormatter>::try_from((cmd.format, address_types, infos))?;
                let default: Option<String> = ffx_target::get_target_specifier(&context).await?;
                writer.line(formatter.lines(default.as_deref()).join("\n"))?;
            }
        }
    }
    Ok(())
}

const DEFAULT_SSH_TIMEOUT_MS: u64 = 10000;

async fn try_get_target_info(
    spec: TargetInfoQuery,
    context: &EnvironmentContext,
) -> Result<(ffx::RemoteControlState, Option<String>, Option<String>), KnockError> {
    let mut resolution = ffx_target::resolve_target_address(&spec, context)
        .await
        .map_err(|e| KnockError::CriticalError(e.into()))?;
    let (rcs_state, pc, bc) = match resolution.identify(context).await {
        Ok(id_result) => (
            ffx::RemoteControlState::Up,
            id_result.product_config.clone(),
            id_result.board_config.clone(),
        ),
        _ => (ffx::RemoteControlState::Down, None, None),
    };
    Ok((rcs_state, pc, bc))
}

async fn get_target_info(
    context: &EnvironmentContext,
    addrs: &[addr::TargetAddr],
) -> Result<(ffx::RemoteControlState, Option<String>, Option<String>)> {
    let ssh_timeout: u64 =
        ffx_config::get("target.host_pipe_ssh_timeout").unwrap_or(DEFAULT_SSH_TIMEOUT_MS);
    let ssh_timeout = Duration::from_millis(ssh_timeout);
    for addr in addrs {
        // An address is, conveniently, a valid target spec as well
        let spec = if addr.port().filter(|x| *x != 0).is_none() {
            format!("{addr}")
        } else {
            format!("{addr}:{}", addr.port().unwrap())
        };
        log::debug!("Trying to make a connection to spec {spec:?}");

        match try_get_target_info(spec.into(), context)
            .on_timeout(ssh_timeout, || {
                Err(KnockError::NonCriticalError(anyhow::anyhow!("knock_rcs() timed out")))
            })
            .await
        {
            Ok(res) => {
                return Ok(res);
            }
            Err(KnockError::NonCriticalError(e)) => {
                log::debug!("Could not connect to {addr:?}: {e:?}");
                continue;
            }
            e => {
                log::debug!("Got error {e:?} when trying to connect to {addr:?}");
                return Ok((ffx::RemoteControlState::Unknown, None, None));
            }
        }
    }
    Ok((ffx::RemoteControlState::Down, None, None))
}

// Convert the handle to a TargetInfo, filling in the information from the target if we are
// asked to make a connection to RCS.
async fn handle_to_info(
    context: &EnvironmentContext,
    handle: discovery::TargetHandle,
    connect_to_target: bool,
) -> Result<ffx::TargetInfo> {
    let (rcs_state, product_config, board_config) =
        if let discovery::TargetState::Product { ref addrs, .. } = handle.state {
            // A let-chain would be cleaner, but they are only available in Rust 2024
            if connect_to_target {
                get_target_info(context, addrs).await?
            } else {
                (ffx::RemoteControlState::Unknown, None, None)
            }
        } else {
            (ffx::RemoteControlState::Unknown, None, None)
        };
    let info: ffx::TargetInfo = handle.into();
    Ok(ffx::TargetInfo { rcs_state: Some(rcs_state), board_config, product_config, ..info })
}

async fn local_list_targets(
    ctx: &EnvironmentContext,
    cmd: &ListCommand,
) -> Result<Vec<ffx::TargetInfo>> {
    let stream = get_handle_stream(cmd, ctx).await?;
    let targets = handles_to_infos(stream, ctx, !cmd.no_probe).await?;
    Ok(targets)
}

async fn handles_to_infos(
    stream: impl futures::Stream<Item = discovery::TargetHandle>,
    ctx: &EnvironmentContext,
    connect: bool,
) -> Result<Vec<fidl_fuchsia_developer_ffx::TargetInfo>> {
    let info_futures = stream.then(|t| handle_to_info(ctx, t, connect));
    let infos: Vec<Result<ffx::TargetInfo>> = info_futures.collect().await;
    let targets = infos.into_iter().collect::<Result<Vec<ffx::TargetInfo>>>()?;
    Ok(targets)
}

async fn get_handle_stream(
    cmd: &ListCommand,
    ctx: &EnvironmentContext,
) -> Result<impl futures::Stream<Item = discovery::TargetHandle>> {
    let name = cmd.nodename.clone();
    let query = TargetInfoQuery::from(name);
    let stream = ffx_target::get_discovery_stream(query, !cmd.no_usb, !cmd.no_mdns, ctx).await?;
    Ok(stream)
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
    use ffx_list_args::Format;
    use ffx_writer::TestBuffers;
    use fidl_fuchsia_developer_ffx as ffx;
    use fidl_fuchsia_developer_ffx::{TargetInfo as FidlTargetInfo, TargetState};
    use regex::Regex;
    use std::net::IpAddr;
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
            target_state: Some(TargetState::Unknown),
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

    async fn try_run_list_test(
        num_tests: usize,
        cmd: ListCommand,
        context: &EnvironmentContext,
        vsock: bool,
    ) -> Result<String> {
        let proxy = setup_fake_target_collection_server(num_tests, vsock);
        let test_buffers = TestBuffers::default();
        let mut writer = VerifiedMachineWriter::new_test(None, &test_buffers);
        let infos = list_targets(proxy, &cmd).await?;
        show_targets(cmd, infos, &mut writer, context).await?;
        Ok(test_buffers.into_stdout_str())
    }

    async fn run_list_test(
        num_tests: usize,
        cmd: ListCommand,
        context: &EnvironmentContext,
        vsock: bool,
    ) -> String {
        try_run_list_test(num_tests, cmd, context, vsock).await.unwrap()
    }

    #[fuchsia::test]
    async fn test_machine_schema() {
        let env = ffx_config::test_init().await.unwrap();
        let proxy = setup_fake_target_collection_server(3, false);
        let test_buffers = TestBuffers::default();
        let mut writer =
            VerifiedMachineWriter::new_test(Some(ffx_writer::Format::Json), &test_buffers);
        let cmd = ListCommand { format: Format::Tabular, ..Default::default() };
        let infos = list_targets(proxy, &cmd).await.expect("list targets");
        show_targets(cmd, infos, &mut writer, &env.context).await.expect("show_targets");
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
        let env = ffx_config::test_init().await.unwrap();
        let proxy = setup_fake_target_collection_server(3, true);
        let test_buffers = TestBuffers::default();
        let mut writer =
            VerifiedMachineWriter::new_test(Some(ffx_writer::Format::Json), &test_buffers);
        let cmd = ListCommand { format: Format::Tabular, ..Default::default() };
        let infos = list_targets(proxy, &cmd).await.expect("list targets");
        show_targets(cmd, infos, &mut writer, &env.context).await.expect("show_targets");
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
        let env = ffx_config::test_init().await.unwrap();
        let output = run_list_test(0, tab_list_cmd(None), &env.context, false).await;
        assert_eq!("".to_string(), output);
        let output = run_list_test(0, tab_list_cmd(None), &env.context, true).await;
        assert_eq!("".to_string(), output);
        Ok(())
    }

    #[fuchsia::test]
    async fn test_list_with_one_device_and_no_nodename() -> Result<()> {
        let env = ffx_config::test_init().await.unwrap();
        let output = run_list_test(1, tab_list_cmd(None), &env.context, false).await;
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
        let env = ffx_config::test_init().await.unwrap();
        let output = run_list_test(1, tab_list_cmd(None), &env.context, true).await;
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
        let env = ffx_config::test_init().await.unwrap();
        let num_tests = 10;
        let output = run_list_test(num_tests, tab_list_cmd(None), &env.context, false).await;
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
        let env = ffx_config::test_init().await.unwrap();
        let output =
            run_list_test(1, tab_list_cmd(Some("Test 0".to_string())), &env.context, false).await;
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
        let env = ffx_config::test_init().await.unwrap();
        let output =
            try_run_list_test(1, tab_list_cmd(Some("blarg".to_string())), &env.context, false)
                .await;
        assert!(output.is_err());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_list_with_multiple_devices_and_not_matching_nodename() -> Result<()> {
        let env = ffx_config::test_init().await.unwrap();
        let num_tests = 25;
        let output = try_run_list_test(
            num_tests,
            tab_list_cmd(Some("blarg".to_string())),
            &env.context,
            false,
        )
        .await;
        assert!(output.is_err());
        Ok(())
    }

    #[fuchsia::test]
    async fn test_list_with_multiple_devices_and_matching_nodename() -> Result<()> {
        let env = ffx_config::test_init().await.unwrap();
        let output =
            run_list_test(25, tab_list_cmd(Some("Test 19".to_string())), &env.context, false).await;
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
        let env = ffx_config::test_init().await.unwrap();
        let num_tests = 25;
        let cmd_none = ListCommand { no_ipv4: true, no_ipv6: true, ..Default::default() };
        let output = try_run_list_test(num_tests, cmd_none, &env.context, false).await;
        assert!(output.is_err());
        Ok(())
    }

    #[test]
    fn test_address_types_from_cmd() -> Result<()> {
        let cmd_none = ListCommand { no_ipv4: true, no_ipv6: true, ..Default::default() };
        assert_eq!(address_types_from_cmd(&cmd_none), AddressTypes::None);
        let cmd_ipv4_only = ListCommand { no_ipv4: false, no_ipv6: true, ..Default::default() };
        assert_eq!(address_types_from_cmd(&cmd_ipv4_only), AddressTypes::Ipv4Only);
        let cmd_ipv6_only = ListCommand { no_ipv4: true, no_ipv6: false, ..Default::default() };
        assert_eq!(address_types_from_cmd(&cmd_ipv6_only), AddressTypes::Ipv6Only);
        let cmd_all = ListCommand { no_ipv4: false, no_ipv6: false, ..Default::default() };
        assert_eq!(address_types_from_cmd(&cmd_all), AddressTypes::All);
        let cmd_all_default = ListCommand::default();
        assert_eq!(address_types_from_cmd(&cmd_all_default), AddressTypes::All);
        Ok(())
    }

    #[fuchsia::test]
    async fn test_sorted_output() -> Result<()> {
        let env = ffx_config::test_init().await.unwrap();
        let cmd = ListCommand::default();
        let test_buffers = TestBuffers::default();
        let mut writer = VerifiedMachineWriter::new_test(None, &test_buffers);
        let ti1 = ffx::TargetInfo {
            nodename: Some(String::from("z")),
            addresses: Some(vec![]),
            rcs_state: Some(ffx::RemoteControlState::Unknown),
            target_state: Some(ffx::TargetState::Unknown),
            ..Default::default()
        };
        let ti2 = ffx::TargetInfo { nodename: Some(String::from("a")), ..ti1.clone() };
        let infos = vec![ti1, ti2];
        show_targets(cmd, infos, &mut writer, &env.context).await?;
        let out: Vec<String> =
            test_buffers.into_stdout_str().lines().map(|s| s.to_string()).collect();
        // Line 0 is the header
        assert!(out[1].starts_with("a"));
        assert!(out[2].starts_with("z"));
        Ok(())
    }

    #[fuchsia::test]
    async fn test_serial_addresses() {
        // USB targets should have an empty list of addresses, not None
        let env = ffx_config::test_init().await.unwrap();
        let handle = discovery::TargetHandle {
            node_name: Some("nodename".to_string()),
            state: discovery::TargetState::Fastboot(discovery::FastbootTargetState {
                serial_number: "12345678".to_string(),
                connection_state: discovery::FastbootConnectionState::Usb,
            }),
            manual: false,
        };
        let stream = futures::stream::once(async { handle });
        let targets = handles_to_infos(stream, &env.context, true).await;
        let targets = targets.unwrap();
        assert_ne!(targets[0].addresses, None);
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
    async fn test_command_target() {
        let ffx_cmd =
            ffx_command::Ffx { target: Some(String::from("mytarget")), ..Default::default() };
        let ffx_cmd_line = ffx_command::FfxCommandLine { global: ffx_cmd, ..Default::default() };
        let env = ffx_config::test_init().await.unwrap();
        let fho_env = fho::FhoEnvironment::new(&env.context, &ffx_cmd_line);
        let tool = build_list_tool(ListCommand::default(), &env, fho_env).await;
        let cmd = tool.update_from_target();
        assert_eq!(cmd.nodename, Some(String::from("mytarget")));
    }

    #[fuchsia::test]
    async fn test_command_target_preserves_arg() {
        let ffx_cmd_line = ffx_command::FfxCommandLine::default();
        let env = ffx_config::test_init().await.unwrap();
        let fho_env = fho::FhoEnvironment::new(&env.context, &ffx_cmd_line);
        let list_cmd =
            ListCommand { nodename: Some(String::from("mytarget")), ..Default::default() };
        let tool = build_list_tool(list_cmd, &env, fho_env).await;
        let cmd = tool.update_from_target();
        assert_eq!(cmd.nodename, Some(String::from("mytarget")));
    }

    #[fuchsia::test]
    async fn test_handle_to_info_address_sorting() {
        let env = ffx_config::test_init().await.unwrap();
        let non_link_local_addr: TargetAddr = "[2001:db8::1]:0".parse().unwrap();
        let link_local_addr: TargetAddr = "[fe80::1]:0".parse().unwrap();

        let handle = discovery::TargetHandle {
            node_name: Some("test-node".to_string()),
            state: discovery::TargetState::Product {
                addrs: vec![non_link_local_addr.clone(), link_local_addr.clone()],
                serial: None,
            },
            manual: false,
        };

        let info = handle_to_info(&env.context, handle, false).await.unwrap();

        let addrs = info.addresses.unwrap();
        assert_eq!(addrs.len(), 2);
        let addrs: Vec<TargetAddr> = addrs.into_iter().map(|a| a.into()).collect();
        // The link-local address should come first.
        assert_eq!(addrs[0], link_local_addr);
        assert_eq!(addrs[1], non_link_local_addr);
    }
}
