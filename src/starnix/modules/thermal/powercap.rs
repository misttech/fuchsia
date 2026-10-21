// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, anyhow};
use fidl_fuchsia_power_powercap as fpowercap;
use starnix_core::device::kobject::Device;
use starnix_core::fs::sysfs::build_device_directory;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::FsNodeOps;
use starnix_core::vfs::pseudo::simple_directory::SimpleDirectoryMutator;
use starnix_core::vfs::pseudo::simple_file::{BytesFile, BytesFileOps};
use starnix_logging::log_warn;
use starnix_uapi::errors::{Errno, errno, error};
use starnix_uapi::file_mode::mode;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const RESERVED_ZONE_PATHS: &[&str] = &[
    ".",
    "..",
    "name",
    "enabled",
    "power_limit_uw",
    "power_limit_bypass",
    "constraint_0_power_limit_uw",
    "constraint_0_name",
    "constraint_0_max_power_uw",
    "constraint_0_min_power_uw",
    "power_levels_uw",
    "power_limit_bypass_msec",
    "uevent",
    "power",
    "subsystem",
    "dev",
];

fn is_valid_zone_path(path: &str) -> bool {
    !path.is_empty() && !path.contains('/') && !RESERVED_ZONE_PATHS.contains(&path)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PowercapZoneConfig {
    node_id: u64,
    parent_node_id: Option<u64>,
    zone_path: String,
    name: String,
    constraint_name: Option<String>,
    max_power_uw: Option<u64>,
    min_power_uw: Option<u64>,
    power_levels_uw: Option<Vec<u64>>,
    supports_bypass: bool,
}

impl PowercapZoneConfig {
    fn from_fidl(info: fpowercap::PowercapNodeInfo) -> Option<Self> {
        let Some(node_id) = info.node_id else {
            log_warn!("PowercapNodeInfo missing node_id, skipping: {:?}", info);
            return None;
        };
        let Some(zone_path) = info.zone_path.filter(|path| is_valid_zone_path(path)) else {
            log_warn!("PowercapNodeInfo for node {} has invalid zone_path, skipping", node_id);
            return None;
        };
        let Some(name) = info.name.filter(|name| !name.is_empty()) else {
            log_warn!("PowercapNodeInfo for zone {} missing name, skipping", zone_path);
            return None;
        };

        Some(Self {
            node_id,
            parent_node_id: info.parent_node_id,
            zone_path,
            name,
            constraint_name: info.constraint_name.filter(|name| !name.is_empty()),
            max_power_uw: info.max_power_uw,
            min_power_uw: info.min_power_uw,
            power_levels_uw: info.power_levels_uw.filter(|levels| !levels.is_empty()),
            supports_bypass: info.supports_bypass.unwrap_or(false),
        })
    }
}

fn build_powercap_zone_directory(
    device: &Device,
    dir: &SimpleDirectoryMutator,
    zone: &PowercapZoneConfig,
    client: Arc<fpowercap::ClientSynchronousProxy>,
) {
    build_device_directory(device, dir);
    dir.entry(
        "name",
        BytesFile::new_node(format!("{}\n", zone.name).into_bytes()),
        mode!(IFREG, 0o444),
    );
    dir.entry(
        "constraint_0_power_limit_uw",
        PowercapAttrFile::new_node(zone.node_id, PowercapAttr::PowerLimit, client.clone()),
        mode!(IFREG, 0o664),
    );

    if let Some(constraint_name) = &zone.constraint_name {
        dir.entry(
            "constraint_0_name",
            BytesFile::new_node(format!("{}\n", constraint_name).into_bytes()),
            mode!(IFREG, 0o444),
        );
    }
    if let Some(max_power_uw) = zone.max_power_uw {
        dir.entry(
            "constraint_0_max_power_uw",
            BytesFile::new_node(format!("{}\n", max_power_uw).into_bytes()),
            mode!(IFREG, 0o444),
        );
    }
    if let Some(min_power_uw) = zone.min_power_uw {
        dir.entry(
            "constraint_0_min_power_uw",
            BytesFile::new_node(format!("{}\n", min_power_uw).into_bytes()),
            mode!(IFREG, 0o444),
        );
    }
    if let Some(power_levels_uw) = &zone.power_levels_uw {
        let levels_str = power_levels_uw.iter().map(u64::to_string).collect::<Vec<_>>().join(" ");
        dir.entry(
            "power_levels_uw",
            BytesFile::new_node(format!("{}\n", levels_str).into_bytes()),
            mode!(IFREG, 0o444),
        );
    }
    if zone.supports_bypass {
        dir.entry(
            "power_limit_bypass_msec",
            PowercapAttrFile::new_node(zone.node_id, PowercapAttr::PowerLimitBypass, client),
            mode!(IFREG, 0o664),
        );
    }
}

const POWERCAP_FIDL_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(5);

fn powercap_fidl_deadline() -> zx::MonotonicInstant {
    zx::MonotonicInstant::after(POWERCAP_FIDL_TIMEOUT)
}

#[derive(Clone, Copy, Debug)]
enum PowercapAttr {
    PowerLimit,
    PowerLimitBypass,
}

impl PowercapAttr {
    fn read_value(
        self,
        client: &fpowercap::ClientSynchronousProxy,
        node_id: u64,
    ) -> Result<u64, Errno> {
        let deadline = powercap_fidl_deadline();
        let result = match self {
            Self::PowerLimit => client.get_power_limit(node_id, deadline),
            Self::PowerLimitBypass => client.get_power_limit_bypass(node_id, deadline),
        };
        map_fidl_result(result, self, "read", node_id)
    }

    fn write_value(
        self,
        client: &fpowercap::ClientSynchronousProxy,
        node_id: u64,
        value: u64,
    ) -> Result<(), Errno> {
        let deadline = powercap_fidl_deadline();
        let result = match self {
            Self::PowerLimit => client.set_power_limit(node_id, value, deadline),
            Self::PowerLimitBypass => client.set_power_limit_bypass(node_id, value, deadline),
        };
        map_fidl_result(result, self, "write", node_id)
    }
}

fn map_fidl_result<T>(
    result: Result<Result<T, fpowercap::PowercapError>, fidl::Error>,
    attr: PowercapAttr,
    op: &str,
    node_id: u64,
) -> Result<T, Errno> {
    match result {
        Ok(Ok(val)) => Ok(val),
        Ok(Err(fpowercap::PowercapError::InvalidArguments)) => error!(EINVAL),
        Ok(Err(err)) => {
            log_warn!("{:?} {} failed for node {}: {:?}", attr, op, node_id, err);
            error!(EIO)
        }
        Err(
            err @ (fidl::Error::ClientCall(zx::Status::TIMED_OUT)
            | fidl::Error::ClientRead(fidl::TransportError::Status(zx::Status::TIMED_OUT))
            | fidl::Error::ClientWrite(fidl::TransportError::Status(zx::Status::TIMED_OUT))),
        ) => {
            log_warn!("{:?} {} FIDL call timed out for node {}: {:?}", attr, op, node_id, err);
            error!(ETIMEDOUT)
        }
        Err(err) => {
            log_warn!("{:?} {} FIDL error for node {}: {:?}", attr, op, node_id, err);
            error!(EIO)
        }
    }
}

fn parse_u64_attr(data: &[u8]) -> Result<u64, Errno> {
    let num_str = std::str::from_utf8(data).map_err(|_| errno!(EINVAL))?;
    num_str.trim().parse().map_err(|_| errno!(EINVAL))
}

struct PowercapAttrFile {
    node_id: u64,
    attr: PowercapAttr,
    client: Arc<fpowercap::ClientSynchronousProxy>,
}

impl PowercapAttrFile {
    fn new_node(
        node_id: u64,
        attr: PowercapAttr,
        client: Arc<fpowercap::ClientSynchronousProxy>,
    ) -> impl FsNodeOps {
        BytesFile::new_node(Self { node_id, attr, client })
    }
}

impl BytesFileOps for PowercapAttrFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let value = self.attr.read_value(&self.client, self.node_id)?;
        Ok(format!("{}\n", value).into_bytes().into())
    }

    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let value = parse_u64_attr(&data)?;
        self.attr.write_value(&self.client, self.node_id, value)
    }
}

/// Validates and topologically sorts `PowercapNodeInfo` entries so parent zones always precede
/// their child zones, skipping duplicate `node_id`s, duplicate `zone_path`s, and orphaned or
/// cyclic `parent_node_id` chains.
fn resolve_powercap_zones(nodes: Vec<fpowercap::PowercapNodeInfo>) -> Vec<PowercapZoneConfig> {
    let mut seen_node_ids = HashSet::new();
    let mut seen_zone_paths = HashSet::new();
    let mut pending = Vec::new();

    for zone in nodes.into_iter().filter_map(PowercapZoneConfig::from_fidl) {
        if seen_node_ids.contains(&zone.node_id) {
            log_warn!(
                "Duplicate powercap node_id {} for zone {}, skipping",
                zone.node_id,
                zone.zone_path
            );
            continue;
        }
        if seen_zone_paths.contains(&zone.zone_path) {
            log_warn!(
                "Duplicate powercap zone_path {} for node_id {}, skipping",
                zone.zone_path,
                zone.node_id
            );
            continue;
        }
        seen_node_ids.insert(zone.node_id);
        seen_zone_paths.insert(zone.zone_path.clone());
        pending.push(zone);
    }

    if pending.is_empty() {
        return Vec::new();
    }

    let mut resolved = Vec::with_capacity(pending.len());
    let mut registered_ids = HashSet::new();

    // Resolve parent zones before child zones so the `/sys/devices/virtual/powercap/...`
    // hierarchy reflects `parent_node_id` regardless of the order returned by `GetPowercapTree`.
    while !pending.is_empty() {
        let mut next_pending = Vec::new();
        let mut made_progress = false;

        for zone in pending {
            if let Some(parent_id) = zone.parent_node_id
                && !registered_ids.contains(&parent_id)
            {
                next_pending.push(zone);
                continue;
            }

            registered_ids.insert(zone.node_id);
            resolved.push(zone);
            made_progress = true;
        }

        if !made_progress {
            for orphan in &next_pending {
                log_warn!(
                    "Powercap zone {} (node_id={}) has unresolved parent_node_id {:?}, skipping",
                    orphan.zone_path,
                    orphan.node_id,
                    orphan.parent_node_id
                );
            }
            break;
        }
        pending = next_pending;
    }

    resolved
}

/// Registers powercap zone devices in `/sys/class/powercap` and `/sys/devices/virtual/powercap`
/// using the provided `fuchsia.power.powercap.Client` proxy.
fn register_powercap_devices_with_client(
    kernel: &Kernel,
    client: Arc<fpowercap::ClientSynchronousProxy>,
) -> Result<(), Error> {
    let nodes = client
        .get_powercap_tree(powercap_fidl_deadline())
        .map_err(|err| anyhow!("Failed to fetch powercap tree: {:?}", err))?;

    let zones = resolve_powercap_zones(nodes);
    if zones.is_empty() {
        return Ok(());
    }

    let registry = &kernel.device_registry;
    let powercap_class = registry.objects.powercap_class();
    let mut registered_devices: HashMap<u64, Device> = HashMap::new();

    for zone in zones {
        let parent_device =
            zone.parent_node_id.and_then(|parent_id| registered_devices.get(&parent_id).cloned());
        let device = registry.add_numberless_device(
            zone.zone_path.as_str().into(),
            parent_device,
            powercap_class.clone(),
            |device, dir| build_powercap_zone_directory(device, dir, &zone, client.clone()),
        );
        registered_devices.insert(zone.node_id, device);
    }

    Ok(())
}

/// Connects to `fuchsia.power.powercap.Client` and populates `/sys/class/powercap`.
///
/// If the protocol is not routed or not served in the current environment, logs a warning and
/// returns cleanly so thermal initialization can proceed on platforms without PowerCap support.
pub(crate) fn powercap_device_init(kernel: &Kernel) {
    let client =
        match fuchsia_component::client::connect_to_protocol_sync::<fpowercap::ClientMarker>() {
            Ok(client) => Arc::new(client),
            Err(err) => {
                log_warn!("Failed to connect to fuchsia.power.powercap.Client: {:?}", err);
                return;
            }
        };

    if let Err(err) = register_powercap_devices_with_client(kernel, client) {
        log_warn!("Skipping powercap sysfs initialization: {:?}", err);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use starnix_core::testing::spawn_kernel_and_run;

    fn make_node(
        node_id: Option<u64>,
        parent_node_id: Option<u64>,
        zone_path: Option<&str>,
        name: Option<&str>,
    ) -> fpowercap::PowercapNodeInfo {
        fpowercap::PowercapNodeInfo {
            node_id,
            parent_node_id,
            zone_path: zone_path.map(String::from),
            name: name.map(String::from),
            ..Default::default()
        }
    }

    fn spawn_fake_powercap_client(
        nodes: Vec<fpowercap::PowercapNodeInfo>,
    ) -> Arc<fpowercap::ClientSynchronousProxy> {
        let (proxy, server_end) = fidl::endpoints::create_sync_proxy::<fpowercap::ClientMarker>();
        std::thread::spawn(move || {
            let mut executor = fuchsia_async::LocalExecutor::default();
            let mut stream = server_end.into_stream();
            executor.run_singlethreaded(async move {
                while let Some(Ok(request)) = stream.next().await {
                    match request {
                        fpowercap::ClientRequest::GetPowercapTree { responder } => {
                            let _ = responder.send(&nodes);
                        }
                        fpowercap::ClientRequest::GetPowerLimit { responder, .. } => {
                            let _ = responder.send(Ok(1_500_000));
                        }
                        fpowercap::ClientRequest::SetPowerLimit { responder, .. } => {
                            let _ = responder.send(Ok(()));
                        }
                        fpowercap::ClientRequest::GetPowerLimitBypass { responder, .. } => {
                            let _ = responder.send(Ok(250));
                        }
                        fpowercap::ClientRequest::SetPowerLimitBypass { responder, .. } => {
                            let _ = responder.send(Ok(()));
                        }
                        fpowercap::ClientRequest::_UnknownMethod { .. } => {}
                    }
                }
            });
        });
        Arc::new(proxy)
    }

    #[fuchsia::test]
    fn test_from_fidl_rejects_missing_fields_and_invalid_zone_paths() {
        assert_eq!(
            PowercapZoneConfig::from_fidl(make_node(None, None, Some("gpc:0"), Some("gpc"))),
            None
        );
        assert_eq!(
            PowercapZoneConfig::from_fidl(make_node(Some(0), None, None, Some("gpc"))),
            None
        );
        assert_eq!(
            PowercapZoneConfig::from_fidl(make_node(Some(0), None, Some(""), Some("gpc"))),
            None
        );
        assert_eq!(
            PowercapZoneConfig::from_fidl(make_node(Some(0), None, Some("gpc/0"), Some("gpc"))),
            None
        );
        assert_eq!(
            PowercapZoneConfig::from_fidl(make_node(Some(0), None, Some("."), Some("gpc"))),
            None
        );
        assert_eq!(
            PowercapZoneConfig::from_fidl(make_node(Some(0), None, Some(".."), Some("gpc"))),
            None
        );
        for reserved in RESERVED_ZONE_PATHS {
            assert_eq!(
                PowercapZoneConfig::from_fidl(make_node(
                    Some(0),
                    None,
                    Some(reserved),
                    Some("gpc")
                )),
                None,
                "Expected reserved attribute name {reserved} to be rejected"
            );
        }
        assert_eq!(
            PowercapZoneConfig::from_fidl(make_node(Some(0), None, Some("gpc:0"), None)),
            None
        );
        assert_eq!(
            PowercapZoneConfig::from_fidl(make_node(Some(0), None, Some("gpc:0"), Some(""))),
            None
        );

        let valid = PowercapZoneConfig::from_fidl(fpowercap::PowercapNodeInfo {
            constraint_name: Some(String::new()),
            power_levels_uw: Some(Vec::new()),
            ..make_node(Some(1), Some(0), Some("gpc:0:0"), Some("TestZone"))
        })
        .expect("valid zone should parse");
        assert_eq!(valid.node_id, 1);
        assert_eq!(valid.parent_node_id, Some(0));
        assert_eq!(valid.zone_path, "gpc:0:0");
        assert_eq!(valid.name, "TestZone");
        assert_eq!(valid.constraint_name, None);
        assert_eq!(valid.power_levels_uw, None);
        assert!(!valid.supports_bypass);
    }

    #[fuchsia::test]
    fn test_resolve_powercap_zones_topological_dedup_orphan_and_cycle() {
        let nodes = vec![
            // Intermediate parent appearing before root.
            make_node(Some(1), Some(0), Some("gpc:0:0"), Some("TestZone")),
            // Duplicate zone_path ("gpc:0:0") using node_id 2 before the valid node 2 entry:
            // must be rejected without poisoning node_id 2 in seen_node_ids.
            make_node(Some(2), Some(1), Some("gpc:0:0"), Some("dup-path-for-node-2")),
            // Valid child node 2 appearing after the rejected duplicate-path entry above.
            make_node(Some(2), Some(1), Some("gpc:0:0:0"), Some("LITTLE")),
            // Duplicate node_id (1) with parent_node_id=None appearing while node 1's parent (0)
            // is still unresolved: must be rejected in the initial deduplication pass without
            // hijacking node 1 or skipping its child (2).
            make_node(Some(1), None, Some("gpc:hijack-id"), Some("hijack-id")),
            // Duplicate zone_path ("gpc:0:0") with parent_node_id=None appearing while node 1's
            // parent (0) is still unresolved: must be rejected in the initial deduplication pass
            // without displacing node 1 ("gpc:0:0") or its child (2).
            make_node(Some(99), None, Some("gpc:0:0"), Some("hijack-path")),
            // Root zone.
            make_node(Some(0), None, Some("gpc:0"), Some("gpc")),
            // Duplicate node_id (0) with a different path: must be skipped.
            make_node(Some(0), None, Some("gpc:dup"), Some("dup-id")),
            // Duplicate zone_path ("gpc:0") with a distinct node_id (10): must be skipped.
            make_node(Some(10), None, Some("gpc:0"), Some("dup-path")),
            // Child of the skipped duplicate-path node (10): must be skipped as unresolved parent.
            make_node(Some(11), Some(10), Some("gpc:0:dup-child"), Some("dup-child")),
            // Orphaned node referencing non-existent parent 999.
            make_node(Some(20), Some(999), Some("gpc:orphan"), Some("orphan")),
            // Cyclic parent_node_id chain (30 <-> 31).
            make_node(Some(30), Some(31), Some("gpc:cycle-a"), Some("cycle-a")),
            make_node(Some(31), Some(30), Some("gpc:cycle-b"), Some("cycle-b")),
        ];

        let resolved = resolve_powercap_zones(nodes);
        let resolved_ids_and_paths: Vec<(u64, &str, &str)> =
            resolved.iter().map(|z| (z.node_id, z.zone_path.as_str(), z.name.as_str())).collect();
        assert_eq!(
            resolved_ids_and_paths,
            vec![(0, "gpc:0", "gpc"), (1, "gpc:0:0", "TestZone"), (2, "gpc:0:0:0", "LITTLE")]
        );
    }

    #[::fuchsia::test]
    async fn test_register_powercap_devices_with_client_empty_and_populated() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let root = &kernel.device_registry.objects.root;

            let empty_client = spawn_fake_powercap_client(Vec::new());
            register_powercap_devices_with_client(kernel, empty_client)
                .expect("empty powercap tree registration should succeed");
            assert!(root.lookup("class/powercap".into()).is_none());
            assert!(root.lookup("devices/virtual/powercap".into()).is_none());

            let populated_client = spawn_fake_powercap_client(vec![
                fpowercap::PowercapNodeInfo {
                    node_id: Some(1),
                    parent_node_id: Some(0),
                    zone_path: Some("gpc:0:0".to_string()),
                    name: Some("TestZone".to_string()),
                    constraint_name: Some("TestConstraint".to_string()),
                    max_power_uw: Some(5_000_000),
                    min_power_uw: Some(500_000),
                    power_levels_uw: Some(vec![5_000_000, 2_500_000, 500_000]),
                    supports_bypass: Some(true),
                    ..Default::default()
                },
                fpowercap::PowercapNodeInfo {
                    node_id: Some(0),
                    parent_node_id: None,
                    zone_path: Some("gpc:0".to_string()),
                    name: Some("gpc".to_string()),
                    power_levels_uw: Some(Vec::new()),
                    supports_bypass: Some(false),
                    ..Default::default()
                },
            ]);
            register_powercap_devices_with_client(kernel, populated_client)
                .expect("populated powercap tree registration should succeed");

            assert!(root.lookup("class/powercap/gpc:0".into()).is_some());
            assert!(root.lookup("class/powercap/gpc:0:0".into()).is_some());
            assert!(root.lookup("devices/virtual/powercap/gpc:0/name".into()).is_some());
            assert!(
                root.lookup("devices/virtual/powercap/gpc:0/constraint_0_power_limit_uw".into())
                    .is_some()
            );
            assert!(root.lookup("devices/virtual/powercap/gpc:0/power_levels_uw".into()).is_none());
            assert!(
                root.lookup("devices/virtual/powercap/gpc:0/power_limit_bypass_msec".into())
                    .is_none()
            );

            assert!(root.lookup("devices/virtual/powercap/gpc:0/gpc:0:0/name".into()).is_some());
            assert!(
                root.lookup(
                    "devices/virtual/powercap/gpc:0/gpc:0:0/constraint_0_power_limit_uw".into()
                )
                .is_some()
            );
            assert!(
                root.lookup("devices/virtual/powercap/gpc:0/gpc:0:0/constraint_0_name".into())
                    .is_some()
            );
            assert!(
                root.lookup(
                    "devices/virtual/powercap/gpc:0/gpc:0:0/constraint_0_max_power_uw".into()
                )
                .is_some()
            );
            assert!(
                root.lookup(
                    "devices/virtual/powercap/gpc:0/gpc:0:0/constraint_0_min_power_uw".into()
                )
                .is_some()
            );
            assert!(
                root.lookup("devices/virtual/powercap/gpc:0/gpc:0:0/power_levels_uw".into())
                    .is_some()
            );
            assert!(
                root.lookup(
                    "devices/virtual/powercap/gpc:0/gpc:0:0/power_limit_bypass_msec".into()
                )
                .is_some()
            );
        })
        .await;
    }

    #[fuchsia::test]
    fn test_parse_u64_attr_and_map_fidl_result() {
        assert_eq!(parse_u64_attr(b"2200000\n"), Ok(2_200_000));
        assert_eq!(parse_u64_attr(b"  0  "), Ok(0));
        assert_eq!(parse_u64_attr(b"invalid"), error!(EINVAL));
        assert_eq!(parse_u64_attr(b"-10"), error!(EINVAL));
        assert_eq!(parse_u64_attr(&[0xff, 0xfe]), error!(EINVAL));

        assert_eq!(
            map_fidl_result(Ok(Ok(123u64)), PowercapAttr::PowerLimit, "read", 1),
            Ok(123u64)
        );
        assert_eq!(
            map_fidl_result::<u64>(
                Ok(Err(fpowercap::PowercapError::InvalidArguments)),
                PowercapAttr::PowerLimit,
                "write",
                1
            ),
            error!(EINVAL)
        );
        assert_eq!(
            map_fidl_result::<u64>(
                Ok(Err(fpowercap::PowercapError::Internal)),
                PowercapAttr::PowerLimit,
                "write",
                1
            ),
            error!(EIO)
        );
        assert_eq!(
            map_fidl_result::<u64>(
                Err(fidl::Error::ClientCall(zx::Status::TIMED_OUT)),
                PowercapAttr::PowerLimit,
                "read",
                1
            ),
            error!(ETIMEDOUT)
        );
        assert_eq!(
            map_fidl_result::<u64>(
                Err(fidl::Error::Invalid),
                PowercapAttr::PowerLimitBypass,
                "read",
                1
            ),
            error!(EIO)
        );
    }
}
