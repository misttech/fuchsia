// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Packet capture management library.

use std::io::Write as _;
use std::path::PathBuf;
use std::str::FromStr as _;

use flex_client::ProxyHasDomain;
use flex_fuchsia_ebpf as febpf;
use flex_fuchsia_io as fio;
use flex_fuchsia_net as fnet;
use flex_fuchsia_net_debug as fnet_debug;
use flex_fuchsia_net_interfaces as fnet_interfaces;
use zx_status;

#[cfg(not(feature = "fdomain"))]
use fidl_fuchsia_net_interfaces_ext as fnet_interfaces_ext;
#[cfg(feature = "fdomain")]
use fidl_fuchsia_net_interfaces_ext_fdomain as fnet_interfaces_ext;

use anyhow::Context as _;
use writer::ToolIO as _;

use crate::CaptureDeps;
use crate::opts::capture::{StartRollingCommand, StopRollingCommand};

/// The default name used for packet captures if none is specified.
const DEFAULT_CAPTURE_NAME: &str = "capture";
const DEFAULT_OUTPUT_SUBDIR: &str = "pcap";

/// Specifier for matching an interface by name, ID, or IP.
#[derive(Debug, PartialEq, Eq)]
enum CaptureInterfaceSpec {
    /// Match by interface ID.
    Id(std::num::NonZeroU64),
    /// Match by interface name.
    Name(String),
    /// Match by IP address.
    Ip(std::net::IpAddr),
}

impl CaptureInterfaceSpec {
    /// Parses an interface specifier string (e.g. "name:lo", "id:1", "ip:127.0.0.1").
    fn parse(s: &str) -> Result<Self, anyhow::Error> {
        if let Some(id_str) = s.strip_prefix("id:") {
            let id = id_str
                .parse::<std::num::NonZeroU64>()
                .context("failed to parse interface ID as non-zero integer")?;
            return Ok(Self::Id(id));
        }
        if let Some(name_str) = s.strip_prefix("name:") {
            return Ok(Self::Name(name_str.to_string()));
        }
        if let Some(ip_str) = s.strip_prefix("ip:") {
            let ip = std::net::IpAddr::from_str(ip_str).context("failed to parse IP address")?;
            return Ok(Self::Ip(ip));
        }
        anyhow::bail!(
            "Invalid interface format. Interface must start with 'id:', 'name:', or 'ip:' prefix \
                (e.g. 'name:lo')"
        )
    }
}

/// Starts a rolling packet capture on the target.
pub async fn start_rolling<C>(connector: &C, cmd: StartRollingCommand) -> Result<(), anyhow::Error>
where
    C: crate::NetCliDepsConnector,
{
    let StartRollingCommand { name, interface, pcap_filter, snap_len, capture_size } = cmd;
    let name = name.unwrap_or_else(|| DEFAULT_CAPTURE_NAME.to_string());

    let spec = CaptureInterfaceSpec::parse(&interface).map_err(|e| {
        crate::user_facing_error(format!("Failed to parse interface specifier: {e:?}"))
    })?;

    let get_interfaces = async {
        let state_proxy = crate::connect_with_context::<fnet_interfaces::StateMarker, _>(connector)
            .await
            .context("Failed to connect to interfaces state")?;
        let stream =
            fnet_interfaces_ext::event_stream_from_state::<fnet_interfaces_ext::AllInterest>(
                &state_proxy,
                fnet_interfaces_ext::WatchOptions {
                    included_addresses: fnet_interfaces_ext::IncludedAddresses::All,
                    ..Default::default()
                },
            )
            .context("Failed to get watcher stream")?;
        fnet_interfaces_ext::existing(
            stream,
            std::collections::HashMap::<
                std::num::NonZeroU64,
                fnet_interfaces_ext::PropertiesAndState<(), fnet_interfaces_ext::AllInterest>,
            >::new(),
        )
        .await
        .context("Failed to list interfaces")
    };
    let interface_id = match spec {
        CaptureInterfaceSpec::Id(id) => id.get(),
        CaptureInterfaceSpec::Name(name) => {
            let id = get_interfaces
                .await?
                .values()
                .find_map(|p| (&p.properties.name == &name).then_some(p.properties.id))
                .ok_or_else(|| {
                    crate::user_facing_error(format!("No interface found with name '{name}'"))
                })?;
            id.get()
        }
        CaptureInterfaceSpec::Ip(ip) => {
            let id = get_interfaces
                .await?
                .values()
                .find_map(|p| {
                    let has_ip = p.properties.addresses.iter().any(|a| match a.addr.addr {
                        fnet::IpAddress::Ipv4(v4) => std::net::IpAddr::V4(v4.addr.into()) == ip,
                        fnet::IpAddress::Ipv6(v6) => std::net::IpAddr::V6(v6.addr.into()) == ip,
                    });
                    has_ip.then_some(p.properties.id)
                })
                .ok_or_else(|| {
                    crate::user_facing_error(format!(
                        "No interface found with IP address '{interface}'"
                    ))
                })?;
            id.get()
        }
    };

    // TODO(https://fxbug.dev/535265151): Support capturing on all interfaces
    // via "any".
    // TODO(https://fxbug.dev/535265209): Support capturing on multiple interfaces.
    let interfaces = fnet_debug::InterfaceSpecifier::InterfaceIds(vec![interface_id]);

    let bpf_program = pcap_filter
        .map(|filter_str| {
            let fidl_fuchsia_ebpf::VerifiedProgram {
                code,
                struct_access_instructions,
                maps,
                __source_breaking,
            } = pcap::compile::compile_filter(&filter_str).map_err(|e| {
                crate::user_facing_error(format!(
                    "Failed to compile pcap filter '{filter_str}': {e:?}"
                ))
            })?;
            assert!(maps.unwrap().is_empty());
            assert!(struct_access_instructions.unwrap().is_empty());
            Ok::<_, anyhow::Error>(febpf::VerifiedProgram {
                code,
                struct_access_instructions: Some(Vec::new()),
                maps: Some(Vec::new()),
                ..Default::default()
            })
        })
        .transpose()?;

    let common_params = fnet_debug::CommonPacketCaptureParams {
        interfaces: Some(interfaces),
        bpf_program,
        snap_len,
        ..Default::default()
    };

    let provider =
        crate::connect_with_context::<fnet_debug::PacketCaptureProviderMarker, _>(connector)
            .await
            .context("Failed to connect to packet capture provider")?;
    let rolling_params =
        fnet_debug::RollingPacketCaptureParams { capture_size, ..Default::default() };
    let rolling_client = provider
        .start_rolling(common_params, &rolling_params)
        .await
        .context("FIDL error calling StartRolling")?
        .map_err(|e| crate::user_facing_error(format!("StartRolling error: {e:?}")))?
        .into_proxy();

    rolling_client
        .detach(&name)
        .await
        .context("FIDL error calling Detach")?
        .map_err(|e| crate::user_facing_error(format!("Detach failed with error: {e:?}")))?;

    Ok(())
}

/// Reconnects, stops, and downloads a rolling packet capture from the target.
pub async fn stop_rolling<C, D>(
    connector: &C,
    deps: &D,
    cmd: StopRollingCommand,
) -> Result<Option<std::path::PathBuf>, anyhow::Error>
where
    C: crate::NetCliDepsConnector,
    D: CaptureDeps,
{
    let StopRollingCommand { name, output, skip_download } = cmd;

    if skip_download && output.is_some() {
        return Err(crate::user_facing_error("Cannot specify both --skip-download and --output"));
    }

    let name = name.unwrap_or_else(|| DEFAULT_CAPTURE_NAME.to_string());

    let provider =
        crate::connect_with_context::<fnet_debug::PacketCaptureProviderMarker, _>(connector)
            .await
            .context("Failed to connect to packet capture provider")?;
    let rolling_client = provider
        .reconnect_rolling(&name)
        .await
        .context("FIDL error calling ReconnectRolling")?
        .map_err(|e| {
            crate::user_facing_error(format!("ReconnectRolling to name '{name}' failed: {e:?}"))
        })?
        .into_proxy();

    if skip_download {
        rolling_client.discard().await.context("Failed to discard rolling pcap")?;
        return Ok(None);
    }

    let output_path = match output {
        Some(path) => PathBuf::from(path),
        None => {
            let dir = std::env::temp_dir().join(DEFAULT_OUTPUT_SUBDIR);
            dir.join(format!("{name}.pcapng"))
        }
    };

    let download_res = async {
        let client = rolling_client.domain();
        let (file_proxy, file_server) = client.create_proxy::<fio::FileMarker>();
        rolling_client.stop_and_download(file_server).context("Failed to call StopAndDownload")?;

        let mut file = deps.create_output_writer(&output_path).map_err(|e| {
            crate::user_facing_error(format!(
                "Failed to create output file {}: {e:?}",
                output_path.display()
            ))
        })?;

        loop {
            let data = file_proxy
                .read(fio::MAX_BUF)
                .await
                .context("FIDL error reading packet capture")?
                .map_err(|e| zx_status::Status::from_raw(e))
                .context("Failed to read packet capture")?;
            if data.is_empty() {
                file.flush().context("Failed to flush packet capture to file")?;
                return Ok(());
            }
            file.write_all(&data).context("Failed to write packet capture to file")?;
        }
    }
    .await;

    if let Err(e) = download_res {
        if let Err(e) = std::fs::remove_file(&output_path) {
            log::warn!("Failed to delete partial file {output_path:?}: {e:?}");
        }
        return Err(e);
    }

    rolling_client.discard().await.context("Failed to discard rolling pcap")?;

    Ok(Some(output_path))
}

pub async fn do_capture<C, D>(
    mut out: writer::JsonWriter<serde_json::Value>,
    crate::opts::capture::Capture { capture_cmd }: crate::opts::capture::Capture,
    connector: &C,
    deps: &D,
) -> Result<(), anyhow::Error>
where
    C: crate::NetCliDepsConnector,
    D: CaptureDeps,
{
    match capture_cmd {
        crate::opts::capture::CaptureEnum::StartRolling(cmd) => start_rolling(connector, cmd).await,
        crate::opts::capture::CaptureEnum::StopRolling(cmd) => {
            let path = stop_rolling(connector, deps, cmd).await?;
            if let Some(path) = path {
                if out.is_machine() {
                    out.machine(&serde_json::json!({ "path": path.display().to_string() }))?;
                } else {
                    out.line(format!("{}", path.display()))?;
                }
            } else {
                if out.is_machine() {
                    out.machine(&serde_json::json!({ "path": null }))?;
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use fuchsia_async as fasync;

    use assert_matches::assert_matches;
    use futures::TryStreamExt;
    use test_case::test_case;

    use super::*;
    use crate::testutil::TestConnector;

    const MOCK_PCAP_DATA: &[u8] = b"mock pcap data";

    struct SharedBuffer {
        buffer: Arc<Mutex<Option<Vec<u8>>>>,
    }

    impl std::io::Write for SharedBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut guard = self.buffer.lock().unwrap();
            guard.as_mut().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[cfg(feature = "fdomain")]
    fn clone_client(client: &flex_client::ClientArg) -> flex_client::ClientArg {
        client.clone()
    }

    #[cfg(not(feature = "fdomain"))]
    fn clone_client(_client: &flex_client::ClientArg) -> flex_client::ClientArg {
        flex_local::local_client_empty()
    }

    // Stateful mock Netstack state to track the active packet capture (quota of 1).
    #[derive(Clone, Default)]
    struct MockState {
        active_capture: Arc<Mutex<Option<String>>>,
    }

    struct TestDeps {
        output_buffer: Arc<Mutex<Option<Vec<u8>>>>,
        written_path: Arc<Mutex<Option<PathBuf>>>,
    }

    impl CaptureDeps for TestDeps {
        type OutputWriter = SharedBuffer;

        fn create_output_writer(
            &self,
            path: &std::path::Path,
        ) -> Result<Self::OutputWriter, anyhow::Error> {
            let mut guard = self.output_buffer.lock().unwrap();
            if guard.is_some() {
                panic!("create_output_writer called more than once");
            }
            *guard = Some(Vec::new());
            *self.written_path.lock().unwrap() = Some(path.to_path_buf());
            Ok(SharedBuffer { buffer: self.output_buffer.clone() })
        }
    }

    async fn handle_state_stream(
        scope: fasync::ScopeHandle,
        mut stream: fnet_interfaces::StateRequestStream,
    ) {
        while let Some(req) = stream.try_next().await.expect("state stream error") {
            let fnet_interfaces::StateRequest::GetWatcher { watcher, .. } = req;
            let _ = scope.spawn(handle_watcher_stream(watcher.into_stream()));
        }
    }

    async fn handle_watcher_stream(mut stream: fnet_interfaces::WatcherRequestStream) {
        // 1. Existing loopback interface 'lo' with IP 127.0.0.1
        let fnet_interfaces::WatcherRequest::Watch { responder } = stream
            .try_next()
            .await
            .expect("watcher stream error")
            .expect("watcher stream closed early");
        let event = fnet_interfaces::Event::Existing(fnet_interfaces::Properties {
            id: Some(1),
            name: Some("lo".to_string()),
            online: Some(true),
            addresses: Some(vec![fnet_interfaces::Address {
                addr: Some(fnet::Subnet {
                    addr: fnet::IpAddress::Ipv4(fnet::Ipv4Address { addr: [127, 0, 0, 1] }),
                    prefix_len: 8,
                }),
                assignment_state: Some(fnet_interfaces::AddressAssignmentState::Assigned),
                valid_until: Some(i64::MAX),
                preferred_lifetime_info: Some(
                    fnet_interfaces::PreferredLifetimeInfo::PreferredUntil(i64::MAX),
                ),
                ..Default::default()
            }]),
            has_default_ipv4_route: Some(false),
            has_default_ipv6_route: Some(false),
            port_class: Some(fnet_interfaces::PortClass::Loopback(fnet_interfaces::Empty)),
            ..Default::default()
        });
        responder.send(&event).unwrap();

        // 2. Idle
        let fnet_interfaces::WatcherRequest::Watch { responder } = stream
            .try_next()
            .await
            .expect("watcher stream error")
            .expect("watcher stream closed early");
        responder.send(&fnet_interfaces::Event::Idle(fnet_interfaces::Empty)).unwrap();

        // Assert EOF: the client (fnet_interfaces_ext::existing) should close the
        // channel after receiving Idle event.
        assert_matches!(stream.try_next().await, Ok(None));
    }

    async fn handle_provider_stream(
        scope: fasync::ScopeHandle,
        client: flex_client::ClientArg,
        mut stream: fnet_debug::PacketCaptureProviderRequestStream,
        state: MockState,
    ) {
        while let Some(req) = stream.try_next().await.expect("provider stream error") {
            match req {
                fnet_debug::PacketCaptureProviderRequest::StartRolling { responder, .. } => {
                    let (client_end, rolling_stream) =
                        client.create_request_stream::<fnet_debug::RollingPacketCaptureMarker>();
                    responder.send(Ok(client_end)).unwrap();
                    let _ = scope.spawn(handle_rolling_stream(
                        scope.clone(),
                        rolling_stream,
                        state.clone(),
                    ));
                }
                fnet_debug::PacketCaptureProviderRequest::ReconnectRolling { name, responder } => {
                    let matches = state.active_capture.lock().unwrap().as_ref() == Some(&name);
                    if matches {
                        let (client_end, rolling_stream) = client
                            .create_request_stream::<fnet_debug::RollingPacketCaptureMarker>(
                        );
                        responder.send(Ok(client_end)).unwrap();
                        let _ = scope.spawn(handle_rolling_stream(
                            scope.clone(),
                            rolling_stream,
                            state.clone(),
                        ));
                    } else {
                        responder
                            .send(Err(fnet_debug::PacketCaptureReconnectError::NotFound))
                            .unwrap();
                    }
                }
            }
        }
    }

    async fn handle_rolling_stream(
        scope: fasync::ScopeHandle,
        mut stream: fnet_debug::RollingPacketCaptureRequestStream,
        state: MockState,
    ) {
        while let Some(req) = stream.try_next().await.expect("rolling stream error") {
            match req {
                fnet_debug::RollingPacketCaptureRequest::Detach {
                    name: detach_name,
                    responder,
                } => {
                    *state.active_capture.lock().unwrap() = Some(detach_name.clone());
                    responder.send(Ok(())).unwrap();
                }
                fnet_debug::RollingPacketCaptureRequest::StopAndDownload {
                    channel,
                    control_handle: _,
                } => {
                    let _ = scope.spawn(handle_file_stream(channel.into_stream()));
                }
                fnet_debug::RollingPacketCaptureRequest::Discard { responder } => {
                    let _ = state.active_capture.lock().unwrap().take();
                    responder.send().unwrap();
                }
            }
        }
    }

    async fn handle_file_stream(mut stream: fio::FileRequestStream) {
        let fio::FileRequest::Read { responder, count: _ } =
            stream.try_next().await.expect("file stream error").expect("file stream closed early")
        else {
            panic!("expected Read request");
        };
        responder.send(Ok(MOCK_PCAP_DATA)).unwrap();

        let fio::FileRequest::Read { responder, count: _ } =
            stream.try_next().await.expect("file stream error").expect("file stream closed early")
        else {
            panic!("expected Read request");
        };
        responder.send(Ok(b"")).unwrap();

        // Assert EOF: the client should close the channel after receiving empty read.
        assert_matches!(stream.try_next().await, Ok(None));
    }

    struct TestParams {
        start_name: Option<&'static str>,
        stop_name: Option<&'static str>,
        interface: &'static str,
        output: Option<&'static str>,
        skip_download: bool,
    }

    async fn run_lifecycle_test(params: TestParams) {
        let TestParams { start_name, stop_name, interface, output, skip_download } = params;

        let client = flex_local::local_client_empty();
        let (interfaces_state, state_stream) =
            client.create_proxy_and_stream::<fnet_interfaces::StateMarker>();
        let (packet_capture_provider, provider_stream) =
            client.create_proxy_and_stream::<fnet_debug::PacketCaptureProviderMarker>();

        let connector = TestConnector {
            interfaces_state: Some(interfaces_state),
            packet_capture_provider: Some(packet_capture_provider),
            ..Default::default()
        };

        let deps = TestDeps {
            output_buffer: Arc::new(Mutex::new(None)),
            written_path: Arc::new(Mutex::new(None)),
        };

        let state = MockState::default();

        let scope = fasync::Scope::new();
        let _ = scope.spawn(handle_state_stream(scope.to_handle(), state_stream));

        let client_clone = clone_client(&client);
        let _ = scope.spawn(handle_provider_stream(
            scope.to_handle(),
            client_clone,
            provider_stream,
            state.clone(),
        ));

        // 1. Start the rolling capture
        let start_cmd = StartRollingCommand {
            name: start_name.map(String::from),
            interface: interface.to_string(),
            pcap_filter: None,
            snap_len: None,
            capture_size: None,
        };

        start_rolling(&connector, start_cmd).await.unwrap();

        // Verify it used start name (or default) in mock server
        let expected_active_name = start_name.unwrap_or(DEFAULT_CAPTURE_NAME);
        assert_eq!(*state.active_capture.lock().unwrap(), Some(expected_active_name.to_string()));

        // 2. Stop the rolling capture
        let stop_cmd = StopRollingCommand {
            name: stop_name.map(String::from),
            output: output.map(String::from),
            skip_download,
        };

        let path = stop_rolling(&connector, &deps, stop_cmd).await.unwrap();

        if skip_download {
            assert_matches!(path, None);
            assert_eq!(*deps.output_buffer.lock().unwrap(), None);
            assert_eq!(*deps.written_path.lock().unwrap(), None);
        } else {
            let path = path.expect("expected path");
            let expected_path = if let Some(out_path) = output {
                PathBuf::from(out_path)
            } else {
                std::env::temp_dir()
                    .join(DEFAULT_OUTPUT_SUBDIR)
                    .join(format!("{expected_active_name}.pcapng"))
            };
            assert_eq!(path, expected_path);
            assert_eq!(deps.written_path.lock().unwrap().as_ref().unwrap(), &expected_path);
            let guard = deps.output_buffer.lock().unwrap();
            assert_eq!(guard.as_ref().unwrap().as_slice(), MOCK_PCAP_DATA);
        }
        assert_eq!(*state.active_capture.lock().unwrap(), None);
    }

    #[test_case("name:lo" ; "by_name")]
    #[test_case("id:1" ; "by_id")]
    #[test_case("ip:127.0.0.1" ; "by_ip")]
    #[fasync::run_singlethreaded(test)]
    async fn test_interface_name(interface: &'static str) {
        run_lifecycle_test(TestParams {
            start_name: None,
            stop_name: None,
            interface,
            output: None,
            skip_download: false,
        })
        .await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_skip_download() {
        run_lifecycle_test(TestParams {
            start_name: None,
            stop_name: None,
            interface: "name:lo",
            output: None,
            skip_download: true,
        })
        .await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_skip_download_with_output_error() {
        let client = flex_local::local_client_empty();
        let (interfaces_state, state_stream) =
            client.create_proxy_and_stream::<fnet_interfaces::StateMarker>();
        let (packet_capture_provider, provider_stream) =
            client.create_proxy_and_stream::<fnet_debug::PacketCaptureProviderMarker>();

        let connector = TestConnector {
            interfaces_state: Some(interfaces_state),
            packet_capture_provider: Some(packet_capture_provider),
            ..Default::default()
        };

        let deps = TestDeps {
            output_buffer: Arc::new(Mutex::new(None)),
            written_path: Arc::new(Mutex::new(None)),
        };

        let state = MockState::default();

        let scope = fasync::Scope::new();
        let _ = scope.spawn(handle_state_stream(scope.to_handle(), state_stream));
        let client_clone = clone_client(&client);
        let _ = scope.spawn(handle_provider_stream(
            scope.to_handle(),
            client_clone,
            provider_stream,
            state.clone(),
        ));

        let start_cmd = StartRollingCommand {
            name: None,
            interface: "name:lo".to_string(),
            pcap_filter: None,
            snap_len: None,
            capture_size: None,
        };
        start_rolling(&connector, start_cmd).await.unwrap();

        let stop_cmd = StopRollingCommand {
            name: None,
            output: Some("custom_path".to_string()),
            skip_download: true,
        };
        let res = stop_rolling(&connector, &deps, stop_cmd).await;
        assert_matches!(
            res,
            Err(e) if e.to_string().contains("Cannot specify both")
        );
    }

    #[test_case(None, None ; "default_name_default_path")]
    #[test_case(Some("test_cap"), None ; "custom_name_default_path")]
    #[test_case(Some("test_cap"), Some("custom_path") ; "custom_name_custom_path")]
    #[fasync::run_singlethreaded(test)]
    async fn test_name_and_output_path(
        capture_name: Option<&'static str>,
        output: Option<&'static str>,
    ) {
        run_lifecycle_test(TestParams {
            start_name: capture_name,
            stop_name: capture_name,
            interface: "name:lo",
            output,
            skip_download: false,
        })
        .await;
    }

    #[test]
    fn test_parse_interface_spec() {
        assert_eq!(
            CaptureInterfaceSpec::parse("id:12").unwrap(),
            CaptureInterfaceSpec::Id(std::num::NonZeroU64::new(12).unwrap())
        );
        assert_matches!(CaptureInterfaceSpec::parse("id:0"), Err(_));
        assert_matches!(CaptureInterfaceSpec::parse("id:abc"), Err(_));

        assert_eq!(
            CaptureInterfaceSpec::parse("name:loopback").unwrap(),
            CaptureInterfaceSpec::Name("loopback".to_string())
        );

        assert_eq!(
            CaptureInterfaceSpec::parse("ip:127.0.0.1").unwrap(),
            CaptureInterfaceSpec::Ip(net_declare::std_ip!("127.0.0.1"))
        );
        assert_eq!(
            CaptureInterfaceSpec::parse("ip:fe80::1").unwrap(),
            CaptureInterfaceSpec::Ip(net_declare::std_ip!("fe80::1"))
        );

        assert_matches!(CaptureInterfaceSpec::parse("ip:invalid_ip"), Err(_));

        assert_matches!(CaptureInterfaceSpec::parse("loopback"), Err(_));
    }
}
