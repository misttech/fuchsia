# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Utilities for querying and matching Fuchsia driver_host processes."""

from collections.abc import Container
from typing import TYPE_CHECKING, Any, Final

from daemon.ffx_utils import run_ffx_json
from shared.protocol.driver_list import DriverHostEntry

__all__ = [
    "TRANSPORT_HAZARD_PATTERNS",
    "build_driver_host_warnings",
    "collect_host_devices",
    "fetch_driver_hosts",
    "fetch_owner_to_monikers",
    "find_transport_sensitive_drivers",
    "match_driver_host",
    "match_host_by_node",
    "match_host_by_pid",
    "match_host_by_query",
    "matches_driver_host_filter",
    "parse_driver_host_entry",
    "short_driver_name",
]

if TYPE_CHECKING:
    from daemon.daemon import Daemon

TRANSPORT_HAZARD_PATTERNS: Final[tuple[str, ...]] = (
    "usb",
    "dwc3",
    "xhci",
    "cdc",
    "rndis",
    "adb",
    "netdevice",
    "virtio_net",
    "ethernet",
    "sdmmc",
    "uart",
    "serial",
    "watchdog",
    "hrtimer",
)


def short_driver_name(driver_url: str) -> str:
    """Extracts the short component or binary name from a Fuchsia driver URL."""
    if "/" in driver_url:
        return driver_url.rsplit("/", 1)[-1]
    return driver_url


def find_transport_sensitive_drivers(drivers: list[str]) -> list[str]:
    """Returns drivers matching transport or watchdog hazard patterns."""
    sensitive: list[str] = []
    for drv in drivers:
        lower = drv.lower()
        if any(pat in lower for pat in TRANSPORT_HAZARD_PATTERNS):
            sensitive.append(drv)
    return sensitive


async def fetch_owner_to_monikers() -> dict[str, list[str]]:
    """Queries `ffx driver node list` on a best-effort basis."""
    try:
        nodes_raw = await run_ffx_json("driver", "node", "list")
    except Exception:
        # Node topology is best-effort if the target does not support
        # `driver node list`.
        return {}

    if not isinstance(nodes_raw, list):
        return {}

    owner_to_monikers: dict[str, list[str]] = {}
    for node in nodes_raw:
        if not isinstance(node, dict):
            continue
        owner = node.get("owner")
        moniker = node.get("moniker")
        if isinstance(owner, str) and isinstance(moniker, str):
            owner_to_monikers.setdefault(owner, []).append(moniker)

    return owner_to_monikers


def collect_host_devices(
    drivers: list[str], owner_to_monikers: dict[str, list[str]]
) -> list[str]:
    """Collects unique device node monikers bound to `drivers` in order."""
    devices: list[str] = []
    for drv in drivers:
        for moniker in owner_to_monikers.get(drv, []):
            if moniker not in devices:
                devices.append(moniker)
    return devices


def parse_driver_host_entry(
    item: Any,
    owner_to_monikers: dict[str, list[str]],
    attached_pids: Container[int],
) -> DriverHostEntry | None:
    """Parses a raw `ffx driver host list` record into a DriverHostEntry."""
    if not isinstance(item, dict):
        return None

    koid = item.get("koid")
    if not isinstance(koid, int):
        return None

    raw_drivers = item.get("drivers") or []
    drivers = [str(d) for d in raw_drivers if d is not None]
    name = str(item.get("name") or f"driver_host:{koid}")
    devices = collect_host_devices(drivers, owner_to_monikers)
    sensitive = find_transport_sensitive_drivers(drivers)

    return DriverHostEntry(
        host_koid=koid,
        host_name=name,
        drivers=drivers,
        devices=devices,
        colocated=len(drivers) > 1,
        attached=koid in attached_pids,
        transport_hazard=bool(sensitive),
        transport_sensitive=sensitive,
    )


async def fetch_driver_hosts(daemon: Daemon) -> list[DriverHostEntry]:
    """Queries `ffx --machine json driver host list` and `driver node list`."""
    hosts_raw = await run_ffx_json("driver", "host", "list")
    if not isinstance(hosts_raw, list):
        raise RuntimeError("Unexpected JSON output from 'ffx driver host list'")

    owner_to_monikers = await fetch_owner_to_monikers()

    entries: list[DriverHostEntry] = []
    for item in hosts_raw:
        entry = parse_driver_host_entry(
            item, owner_to_monikers, daemon.processes
        )
        if entry is not None:
            entries.append(entry)

    return entries


def matches_driver_host_filter(host: DriverHostEntry, needle: str) -> bool:
    """Returns True if `needle` (lowercased) matches any field of `host`."""
    if needle in str(host.host_koid) or needle in host.host_name.lower():
        return True
    if any(needle in d.lower() for d in host.drivers):
        return True
    return any(needle in m.lower() for m in host.devices)


def _first_driver(host: DriverHostEntry) -> str | None:
    """Returns the primary driver URL for `host`, or None if empty."""
    return host.drivers[0] if host.drivers else None


def match_host_by_pid(
    hosts: list[DriverHostEntry], pid: int
) -> tuple[DriverHostEntry | None, str | None]:
    """Finds a driver_host with `host_koid == pid`.

    Returns:
        A tuple of `(matched_host, primary_driver_url)`. For PID matching,
        the second element is the host's first driver.
    """
    for host in hosts:
        if host.host_koid == pid:
            return host, _first_driver(host)
    return None, None


def match_host_by_node(
    hosts: list[DriverHostEntry], node_query: str
) -> tuple[DriverHostEntry | None, str | None]:
    """Finds a driver_host by device node moniker (exact match, then substring).

    Returns:
        A tuple of `(matched_host, primary_driver_url)`. For node matching,
        the second element is the host's first driver.
    """
    needle = node_query.strip().lower()
    if not needle:
        return None, None

    for host in hosts:
        for dev in host.devices:
            if dev.lower() == needle:
                return host, _first_driver(host)

    for host in hosts:
        for dev in host.devices:
            if needle in dev.lower():
                return host, _first_driver(host)

    return None, None


def match_host_by_query(
    hosts: list[DriverHostEntry],
    query: str,
    *,
    include_nodes_and_host_name: bool = False,
) -> tuple[DriverHostEntry | None, str | None]:
    """Finds a driver_host by driver URL/name, or optionally by node/host name."""
    needle = query.strip().lower()
    if not needle:
        return None, None

    # 1. Exact short-name, stem, or full URL match on driver URL.
    for host in hosts:
        for drv in host.drivers:
            short = short_driver_name(drv).lower()
            stem = short.removesuffix(".cm").removesuffix(".so")
            if needle in (drv.lower(), short, stem):
                return host, drv

    # 2. Substring match on driver URL.
    for host in hosts:
        for drv in host.drivers:
            if needle in drv.lower():
                return host, drv

    # 3. When general query matching is requested, also fall back to host_name/devices.
    if include_nodes_and_host_name:
        for host in hosts:
            if needle in host.host_name.lower() or any(
                needle in dev.lower() for dev in host.devices
            ):
                return host, _first_driver(host)

    return None, None


def match_driver_host(
    hosts: list[DriverHostEntry],
    *,
    driver: str | None = None,
    node: str | None = None,
    pid: int | None = None,
) -> tuple[DriverHostEntry | None, str | None]:
    """Resolves a driver selector (`driver`, `node`, or `pid`) against `hosts`.

    Returns:
        A `(matched_host, matched_driver_url)` tuple. For `driver` selectors,
        the second element is the matched driver URL. For `node` or `pid`
        selectors, it is the host's first (primary) driver.
    """
    if pid is not None:
        return match_host_by_pid(hosts, pid)

    if node is not None:
        return match_host_by_node(hosts, node)

    if driver is not None:
        if driver.isdigit():
            matched, drv = match_host_by_pid(hosts, int(driver))
            if matched is not None:
                return matched, drv
        return match_host_by_query(
            hosts, driver, include_nodes_and_host_name=False
        )

    return None, None


def build_driver_host_warnings(
    host: DriverHostEntry,
) -> tuple[str | None, str | None]:
    """Constructs transport-hazard and colocation warnings for `host`."""
    hazard_warning: str | None = None
    colocated_warning: str | None = None

    if host.transport_hazard:
        short_list = ", ".join(
            short_driver_name(d) for d in host.transport_sensitive
        )
        hazard_warning = (
            f"Host {host.host_koid} colocates transport/watchdog drivers "
            f"({short_list}). Prefer --weak attach and non-stopping inspection "
            "commands (modules, aspace, handles, mem-read) over stopping "
            "breakpoints to avoid severing ffx."
        )

    if len(host.drivers) > 1:
        colocated_warning = (
            f"Host {host.host_koid} colocates {len(host.drivers)} drivers. "
            "Stopping a thread in this process may affect colocated drivers."
        )

    return hazard_warning, colocated_warning
