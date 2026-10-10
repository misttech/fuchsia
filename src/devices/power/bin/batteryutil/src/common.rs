// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Common formatting, path, and service discovery helpers for batteryutil.

use anyhow::{Result, anyhow};
use futures::StreamExt;
use futures::stream::BoxStream;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};

/// A single status update or error from a watched battery or charger service instance.
pub struct WatchEvent {
    pub kind: &'static str,
    pub instance: String,
    pub status: Result<String>,
}

/// A boxed stream of [`WatchEvent`] updates.
pub type WatchEventStream = BoxStream<'static, WatchEvent>;

pub const MEMBER_BATTERY: &str = "battery";
pub const MEMBER_DEVICE: &str = "device";
pub const MEMBER_CHARGER: &str = "charger";
pub const MEMBER_CONTROLLER: &str = "controller";
pub const MEMBER_DEBUG: &str = "debug";

const KNOWN_MEMBER_SUFFIXES: &[&str] =
    &[MEMBER_BATTERY, MEMBER_CHARGER, MEMBER_CONTROLLER, MEMBER_DEBUG, MEMBER_DEVICE];

/// Parses a user-supplied string representation of a current or voltage into integer micro-units
/// (`uA` or `uV`). Supports raw micro-units (`"2500000"`, `"2500000uA"`), milli-units
/// (`"2500mA"`, `"4350mV"`), or base units (`"2.5A"`, `"4.35V"`).
pub fn parse_micro_value(input: &str, expected_unit: &str) -> Result<u32> {
    let s = input.trim();
    let unit_lower = expected_unit.to_ascii_lowercase();
    let s_lower = s.to_ascii_lowercase();

    let (num_str, multiplier) = if let Some(prefix) = s_lower.strip_suffix(&unit_lower) {
        if let Some(stripped) = prefix.strip_suffix('u') {
            (&s[..stripped.len()], 1.0)
        } else if let Some(stripped) = prefix.strip_suffix('m') {
            (&s[..stripped.len()], 1_000.0)
        } else {
            (&s[..prefix.len()], 1_000_000.0)
        }
    } else {
        (s, 1.0)
    };

    let num: f64 =
        num_str.trim().parse().map_err(|_| anyhow!("Invalid number format in '{input}'"))?;
    if !num.is_finite() {
        anyhow::bail!("Non-finite values are not permitted: {input}");
    }
    if num < 0.0 {
        anyhow::bail!("Negative values are not permitted: {input}");
    }

    let val = (num * multiplier).round();
    if val > u32::MAX as f64 {
        anyhow::bail!("Value exceeds maximum 32-bit limit: {input}");
    }

    Ok(val as u32)
}

/// Formats micro-units (uA, uAh, uV) into human-readable quantities with appropriate prefixes.
#[derive(Debug, PartialEq)]
pub struct MicroUnit<T>(pub T, pub &'static str);

impl<T: Into<i64> + Copy> fmt::Display for MicroUnit<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let raw: i64 = self.0.into();
        let sign = if raw < 0 { "-" } else { "" };
        let val: f64 = (raw as f64).abs();
        let (scaled, prefix) = if val >= 1_000_000.0 {
            (val / 1_000_000.0, "")
        } else if val >= 1_000.0 {
            (val / 1_000.0, "m")
        } else {
            (val, "u")
        };
        write!(f, "{}{:.3} {}{}", sign, scaled, prefix, self.1)
    }
}

/// Lists all service instance directory paths found under `/svc/<service_name>`.
pub fn discover_instances(service_name: impl AsRef<str>) -> Vec<String> {
    let service_name = service_name.as_ref();
    let service_dir = Path::new("/svc").join(service_name);
    let mut instances: Vec<String> = std::fs::read_dir(&service_dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.path().into_os_string().into_string().ok())
        .collect();
    instances.sort();
    instances
}

/// Discovers available service instances in `/svc/<service_name>` and selects the first instance if not specified.
pub fn select_instance(service_name: impl AsRef<str>) -> Result<String> {
    let service_name = service_name.as_ref();
    let instances = discover_instances(service_name);

    if instances.is_empty() {
        return Err(anyhow!("No instances found for {}", service_name));
    }

    if instances.len() == 1 {
        return Ok(instances[0].clone());
    }

    eprintln!("Multiple service instances found for {}:", service_name);
    for (i, inst) in instances.iter().enumerate() {
        eprintln!("  {}. {}", i + 1, inst);
    }
    eprintln!(
        "Defaulting to first instance: {} (use -p to specify a different instance)",
        instances[0]
    );
    Ok(instances[0].clone())
}

/// Appends a member protocol suffix (e.g. `battery` or `charger`) if not already present, stripping
/// any preexisting sibling member suffixes (`battery`, `charger`, `controller`, `device`).
pub fn append_member_suffix(path: impl AsRef<Path>, member_suffix: &str) -> PathBuf {
    let mut path = path.as_ref();
    let member = member_suffix.trim_start_matches('/');

    if let Some(file_name) = path.file_name().and_then(|f| f.to_str()) {
        if KNOWN_MEMBER_SUFFIXES.contains(&file_name) {
            if let Some(parent) = path.parent() {
                path = parent;
            }
        }
    }

    path.join(member)
}

/// Runs `query` for one service instance, framing its output with a `title` header when
/// `show_header` is set. Returns whether the query succeeded; a failure is reported on stderr so
/// that the remaining instances are still queried.
pub async fn report_instance(
    title: &str,
    inst: &str,
    show_header: bool,
    query: impl Future<Output = Result<()>>,
) -> bool {
    if show_header {
        println!("=== {title} ({inst}) ===");
    }
    let succeeded = query.await.inspect_err(|e| eprintln!("Error querying {inst}: {e:#}")).is_ok();
    if show_header {
        println!();
    }
    succeeded
}

/// Multiplexes `streams` and prints each status update until all streams close.
pub async fn run_watch_streams(streams: Vec<WatchEventStream>, label: &str) -> Result<()> {
    println!("Watching {label} events on all instances (press Ctrl+C to exit)...\n");
    let mut combined = futures::stream::select_all(streams);
    let mut received_any = false;
    let mut last_err = None;
    while let Some(event) = combined.next().await {
        match event.status {
            Ok(formatted) => {
                received_any = true;
                println!("=== {} Telemetry Update ({}) ===", event.kind, event.instance);
                print!("{formatted}");
                println!();
            }
            Err(e) => {
                eprintln!(
                    "Warning ({}): received {} watch error: {e:#}",
                    event.instance,
                    event.kind.to_lowercase()
                );
                last_err = Some(e);
            }
        }
    }
    if !received_any {
        return Err(last_err.unwrap_or_else(|| anyhow!("All {label} watch streams closed")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    async fn test_report_instance_returns_query_outcome() {
        assert!(report_instance("Telemetry", "ok", true, async { Ok(()) }).await);
        assert!(!report_instance("Telemetry", "err", true, async { Err(anyhow!("boom")) }).await);
    }

    #[test]
    fn test_parse_micro_value() {
        assert_eq!(parse_micro_value("2500000", "A").unwrap(), 2_500_000);
        assert_eq!(parse_micro_value("2500000uA", "A").unwrap(), 2_500_000);
        assert_eq!(parse_micro_value("2500mA", "A").unwrap(), 2_500_000);
        assert_eq!(parse_micro_value("2.5A", "A").unwrap(), 2_500_000);
        assert_eq!(parse_micro_value(" 1.5 a ", "A").unwrap(), 1_500_000);

        assert_eq!(parse_micro_value("4350000", "V").unwrap(), 4_350_000);
        assert_eq!(parse_micro_value("4350000uv", "V").unwrap(), 4_350_000);
        assert_eq!(parse_micro_value("4350mV", "V").unwrap(), 4_350_000);
        assert_eq!(parse_micro_value("4.35V", "V").unwrap(), 4_350_000);

        assert!(parse_micro_value("-1.0A", "A").is_err());
        assert!(parse_micro_value("NaN", "A").is_err());
        assert!(parse_micro_value("infA", "A").is_err());
        assert!(parse_micro_value("5000A", "A").is_err());
        assert!(parse_micro_value("4.35V", "A").is_err());
        assert!(parse_micro_value("invalid", "A").is_err());
    }

    #[test]
    fn test_format_unit() {
        assert_eq!(MicroUnit(500i64, "A").to_string(), "500.000 uA");
        assert_eq!(MicroUnit(1_500i64, "A").to_string(), "1.500 mA");
        assert_eq!(MicroUnit(2_500_000i64, "A").to_string(), "2.500 A");
        assert_eq!(MicroUnit(3_774_000i64, "V").to_string(), "3.774 V");
        assert_eq!(MicroUnit(5_040_000i64, "Ah").to_string(), "5.040 Ah");

        // Negative values
        assert_eq!(MicroUnit(-520_312i64, "A").to_string(), "-520.312 mA");
        assert_eq!(MicroUnit(-1_500_000i64, "A").to_string(), "-1.500 A");
    }

    #[test]
    fn test_append_member_suffix() {
        assert_eq!(
            append_member_suffix("/svc/fuchsia.hardware.power.battery.Service/default", "battery"),
            PathBuf::from("/svc/fuchsia.hardware.power.battery.Service/default/battery")
        );
        assert_eq!(
            append_member_suffix(
                "/svc/fuchsia.hardware.power.battery.Service/default/battery",
                "battery"
            ),
            PathBuf::from("/svc/fuchsia.hardware.power.battery.Service/default/battery")
        );
        assert_eq!(
            append_member_suffix("/svc/fuchsia.hardware.power.charger.Service/default", "/charger"),
            PathBuf::from("/svc/fuchsia.hardware.power.charger.Service/default/charger")
        );
        assert_eq!(
            append_member_suffix("/svc/my-battery/default", "battery"),
            PathBuf::from("/svc/my-battery/default/battery")
        );
        // Sibling suffix replacement (controller -> debug, charger -> debug, device -> battery)
        assert_eq!(
            append_member_suffix(
                "/svc/fuchsia.hardware.power.charger.Service/default/controller",
                "debug"
            ),
            PathBuf::from("/svc/fuchsia.hardware.power.charger.Service/default/debug")
        );
        assert_eq!(
            append_member_suffix(
                "/svc/fuchsia.hardware.power.charger.Service/default/charger",
                "debug"
            ),
            PathBuf::from("/svc/fuchsia.hardware.power.charger.Service/default/debug")
        );
        assert_eq!(
            append_member_suffix(
                "/svc/fuchsia.power.battery.InfoService/default/device",
                "battery"
            ),
            PathBuf::from("/svc/fuchsia.power.battery.InfoService/default/battery")
        );
    }
}
