// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Context as _;
use fdomain_client::fidl::{DiscoverableProtocolMarker, Proxy as _};
use fdomain_fuchsia_diagnostics::{
    ClientSelectorConfiguration, DataType, Format, StreamMode, StreamParameters,
};
use fdomain_fuchsia_diagnostics_host::ArchiveAccessorMarker;
use fdomain_fuchsia_usb_policy as usb_policy;
use ffx_writer::{MachineWriter, ToolIO as _};
use fho::Result;
use futures::AsyncReadExt as _;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use target_holders::RemoteControlProxyHolder;

use crate::{DiagnosticsCommand, device_state_to_str};

const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(fho::FfxTool)]
pub struct DiagnosticsTool {
    #[command]
    pub cmd: DiagnosticsCommand,
    pub rcs_holder: fho::Result<RemoteControlProxyHolder>,
}

#[async_trait::async_trait(?Send)]
impl fho::FfxMain for DiagnosticsTool {
    type Writer = MachineWriter<DiagnosticsOutput>;
    type Error = fho::Error;

    async fn main(self, mut writer: Self::Writer) -> Result<()> {
        let out = self.run(&mut writer).await?;
        writer.machine(&out)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsbHealthReport {
    pub controller_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<u8>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct DiagnosticsOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<UsbHealthReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inspect: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

impl DiagnosticsTool {
    pub async fn run(
        &self,
        writer: &mut MachineWriter<DiagnosticsOutput>,
    ) -> Result<DiagnosticsOutput> {
        let mut out = DiagnosticsOutput::default();

        let default_mode = !self.cmd.health && !self.cmd.inspect && !self.cmd.all;
        let run_health = self.cmd.health || self.cmd.all || default_mode;
        let run_inspect = self.cmd.inspect || self.cmd.all;

        let rcs = self.rcs_holder.as_ref().map_err(|e| {
            fho::Error::User(anyhow::anyhow!(
                "Target connection required for USB diagnostics; target is offline or RCS is unreachable: {e}\n\n\
                 If you need to check on a device that is not responding, \
                 you can run `usb-cli health` in a serial console."
            ))
        })?;

        if run_health {
            if !writer.is_machine() {
                writer.line("\n=== USB Health Report ===")?;
            }
            match query_usb_health(rcs).await {
                Ok(report) => {
                    if !writer.is_machine() {
                        writer.line(format!("  Controller State: {}", report.controller_state))?;
                        if let Some(addr) = report.address {
                            writer.line(format!("  Device Address:   {}", addr))?;
                        } else {
                            writer.line("  Device Address:   None / Unassigned")?;
                        }
                    }
                    out.health = Some(report);
                }
                Err(e) => {
                    let err_str = format!("Could not retrieve USB Health Report: {e}");
                    if !writer.is_machine() {
                        writer.line(&err_str)?;
                    }
                    out.errors.push(err_str);
                }
            }
        }

        if run_inspect {
            if !writer.is_machine() {
                writer.line("\n=== Device-Side USB Inspect Diagnostics ===")?;
            }
            match query_usb_inspect(rcs).await {
                Ok((inspect_items, parse_errors)) => {
                    for err in parse_errors {
                        out.errors.push(err);
                    }
                    if !writer.is_machine() {
                        if inspect_items.is_empty() {
                            writer.line("  No USB-related Inspect monikers found.")?;
                        } else {
                            for item in &inspect_items {
                                if let Some(moniker) =
                                    item.pointer("/moniker").and_then(|m| m.as_str())
                                {
                                    writer.line(format!("\n--- Moniker: {} ---", moniker))?;
                                    if let Some(payload) = item.get("payload") {
                                        let pretty = serde_json::to_string_pretty(payload)
                                            .unwrap_or_else(|_| "[]".to_string());
                                        for line in pretty.lines() {
                                            writer.line(format!("  {line}"))?;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    out.inspect = Some(inspect_items);
                }

                Err(e) => {
                    let err_str = format!("Could not retrieve USB Inspect Diagnostics: {e:#}");
                    if !writer.is_machine() {
                        writer.line(&err_str)?;
                    }
                    out.errors.push(err_str);
                }
            }
        }

        Ok(out)
    }
}

fn is_usb_moniker(moniker: &str) -> bool {
    let m = moniker.to_lowercase();
    m.contains("usb")
        || m.contains("dwc")
        || m.contains("xhci")
        || m.contains("ehci")
        || m.contains("ohci")
        || m.contains("cdc")
        || m.contains("rndis")
}

pub(crate) fn parse_json_diagnostics_stream(
    raw_bytes: &[u8],
    stream_label: &str,
) -> anyhow::Result<(Vec<serde_json::Value>, Vec<String>)> {
    if raw_bytes.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    let mut items = Vec::new();
    let mut errors = Vec::new();

    let stream = serde_json::Deserializer::from_slice(raw_bytes).into_iter::<serde_json::Value>();
    for res in stream {
        match res {
            Ok(serde_json::Value::Array(arr)) => items.extend(arr),
            Ok(val) => items.push(val),
            Err(e) => errors.push(format!("JSON parsing error in {stream_label} stream: {e}")),
        }
    }

    if items.is_empty() && !errors.is_empty() {
        let preview_len = std::cmp::min(raw_bytes.len(), 500);
        let preview = String::from_utf8_lossy(&raw_bytes[..preview_len]);
        anyhow::bail!(
            "Parsing JSON {} snapshot failed ({} bytes received, {} items parsed, errors: {:?}). Payload preview: {:?}",
            stream_label,
            raw_bytes.len(),
            items.len(),
            errors,
            preview
        );
    }

    Ok((items, errors))
}

pub(crate) async fn connect_target_protocol<P: DiscoverableProtocolMarker>(
    rcs: &RemoteControlProxyHolder,
    primary_moniker: &str,
) -> anyhow::Result<P::Proxy> {
    match rcs_fdomain::connect_to_protocol::<P>(TIMEOUT, primary_moniker, rcs).await {
        Ok(proxy) => Ok(proxy),
        Err(_) => {
            rcs_fdomain::toolbox::connect_with_timeout::<P>(rcs, TIMEOUT).await.with_context(|| {
                format!("Connecting to {} at {primary_moniker} or /toolbox", P::PROTOCOL_NAME)
            })
        }
    }
}

pub(crate) async fn query_usb_health(
    rcs: &RemoteControlProxyHolder,
) -> anyhow::Result<UsbHealthReport> {
    let health_proxy =
        connect_target_protocol::<usb_policy::HealthMarker>(rcs, "core/usb-policy").await?;

    let report_res = health_proxy
        .get_report()
        .await
        .context("Calling get_report on HealthProxy")?
        .map_err(|status| anyhow::anyhow!("get_report returned ZX_STATUS {status}"))?;

    Ok(UsbHealthReport {
        controller_state: device_state_to_str(report_res.state).to_string(),
        address: report_res.address,
    })
}

pub(crate) async fn connect_archive_accessor(
    rcs: &RemoteControlProxyHolder,
) -> anyhow::Result<fdomain_fuchsia_diagnostics_host::ArchiveAccessorProxy> {
    connect_target_protocol::<ArchiveAccessorMarker>(
        rcs,
        "core/diagnostics-accessors/archive-accessor",
    )
    .await
}

pub(crate) async fn query_usb_inspect(
    rcs: &RemoteControlProxyHolder,
) -> anyhow::Result<(Vec<serde_json::Value>, Vec<String>)> {
    let accessor = connect_archive_accessor(rcs).await?;

    let params = StreamParameters {
        stream_mode: Some(StreamMode::Snapshot),
        data_type: Some(DataType::Inspect),
        format: Some(Format::Json),
        client_selector_configuration: Some(ClientSelectorConfiguration::SelectAll(true)),
        ..Default::default()
    };

    let (mut client, server) = rcs.domain().create_stream_socket();
    accessor
        .stream_diagnostics(&params, server)
        .await
        .context("Invoking stream_diagnostics on ArchiveAccessor")?;

    let mut raw_bytes = Vec::new();
    client.read_to_end(&mut raw_bytes).await.context("Reading diagnostics stream from socket")?;

    let (items, errors) = parse_json_diagnostics_stream(&raw_bytes, "Inspect")?;

    let filtered: Vec<_> = items
        .into_iter()
        .filter(|item| {
            item.pointer("/moniker").and_then(|m| m.as_str()).is_some_and(is_usb_moniker)
        })
        .collect();

    Ok((filtered, errors))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_usb_moniker() {
        assert!(is_usb_moniker("core/usb-policy"));
        assert!(is_usb_moniker("bootstrap/driver_manager:dwc3"));
        assert!(is_usb_moniker("bootstrap/driver_manager:usb-peripheral"));
        assert!(is_usb_moniker("bootstrap/driver_manager:xhci"));
        assert!(!is_usb_moniker("core/power_policy"));
        assert!(!is_usb_moniker("core/bluetooth_peripheral"));
        assert!(!is_usb_moniker("core/audio"));
    }

    #[test]
    fn test_parse_json_diagnostics_stream_empty() {
        let (items, errors) = parse_json_diagnostics_stream(b"", "Inspect").unwrap();
        assert!(items.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn test_parse_json_diagnostics_stream_valid() {
        let json_data = br#"[{"moniker":"core/usb-policy","payload":{"root":{}}}]"#;
        let (items, errors) = parse_json_diagnostics_stream(json_data, "Inspect").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["moniker"], "core/usb-policy");
        assert!(errors.is_empty());
    }

    #[test]
    fn test_parse_json_diagnostics_stream_partial_errors() {
        let json_data = b"{\"moniker\":\"core/usb-policy\"}\n{invalid_json}";
        let (items, errors) = parse_json_diagnostics_stream(json_data, "Inspect").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["moniker"], "core/usb-policy");
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn test_parse_json_diagnostics_stream_all_invalid() {
        let json_data = b"{invalid_json}";
        assert!(parse_json_diagnostics_stream(json_data, "Inspect").is_err());
    }
}
