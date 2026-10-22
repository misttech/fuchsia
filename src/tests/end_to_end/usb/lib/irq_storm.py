# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Detects unhandled IRQ storms and driver dispatcher spin-loops on a DUT.

An unhandled interrupt storm or tight driver poll loop (for example, DWC3
failing to mask GEVNTSIZ on suspend/disconnect while GEVNTCOUNT is 0, or a
driver re-arming a level-triggered interrupt without clearing the hardware
source) may not prevent USB re-enumeration on every iteration, or may only
surface later as a flaky reconnect timeout.

This module provides deterministic detection across two complementary system
layers:
1. Driver Runtime dispatcher request counts across all drivers via
   `fuchsia.driver.development.Manager/GetDriverHostInfo`
   (`DispatcherInfo.debug_stats.num_total_requests`).
2. Kernel hardware interrupt counts across all CPUs via
   `fuchsia.kernel.Stats/GetCpuStats` (`PerCpuStats.ints`).

Both queries are required to succeed and return non-empty telemetry so that
broken FIDL endpoints or missing capabilities fail loudly rather than silently
skipping checks.

TODO(b/568334574): Move the generic dispatcher and kernel IRQ storm detection
out of `usb/lib` into `src/tests/end_to_end/lib/` so non-USB end-to-end tests
can share it directly.
"""

import dataclasses
import logging
import time
from typing import Any

import fidl_fuchsia_developer_remotecontrol as fd_remotecontrol
import fidl_fuchsia_driver_development as f_driver_development
import fidl_fuchsia_kernel as f_kernel
import fidl_fuchsia_sys2 as f_sys2
from honeydew.typing import custom_types
from mobly import asserts

_LOGGER: logging.Logger = logging.getLogger(__name__)

# Driver Runtime per-dispatcher thresholds:
# - Normal disconnect/reconnect baseline on Iris peaks at ~150-300 req/s steady
#   state and ~1,300 req/s during post-boot UFS storage flush.
# - An unmasked level-triggered IRQ or tight PostTask spin-loop runs at 3-20 us
#   per turn (50,000-300,000+ req/s).
# - 5,000 req/s gives ~4x headroom over peak storage flush (>20x over steady
#   state), paired with a 10,000-request minimum delta so short disconnect
#   windows (e.g. 1-2s) do not false-positive on normal reconnect bursts.
DEFAULT_MAX_DISPATCHER_REQUEST_RATE_HZ: float = 5000.0
DEFAULT_MIN_DISPATCHER_REQUEST_DELTA: int = 10000

# Kernel hardware interrupt thresholds (`PerCpuStats.ints` across all CPUs,
# which already excludes timer interrupts and IPIs):
# - Normal disconnect/reconnect baseline on Iris is ~270-1,040 irq/s total.
# - A hardware IRQ storm on an unmasked GIC line fires at 60,000-500,000 irq/s.
# - 10,000 irq/s (with a 20,000-IRQ minimum delta) gives ~10x headroom over
#   normal peak system activity while catching sustained hardware IRQ storms.
DEFAULT_MAX_HW_IRQ_RATE_HZ: float = 10000.0
DEFAULT_MIN_HW_IRQ_DELTA: int = 20000

_DRIVER_DEVELOPMENT_ENDPOINT = custom_types.FidlEndpoint(
    "/bootstrap/driver_manager",
    "fuchsia.driver.development.Manager",
)
_KERNEL_STATS_MONIKER: str = "/core/remote-control"
_KERNEL_STATS_CAPABILITY_NAME: str = "svc/fuchsia.kernel.Stats"


@dataclasses.dataclass(frozen=True)
class DispatcherSnapshot:
    """Point-in-time request counter for a single Driver Runtime dispatcher."""

    driver_host_name: str
    process_koid: int
    driver_url: str
    dispatcher_name: str
    dispatcher_ptr: int
    num_total_requests: int

    @property
    def key(self) -> tuple[int, int]:
        """Unique identifier for this dispatcher instance within a boot."""
        return (self.process_koid, self.dispatcher_ptr)


@dataclasses.dataclass(frozen=True)
class IrqStormSnapshot:
    """Point-in-time snapshot of driver dispatchers and kernel IRQs."""

    timestamp_monotonic_sec: float
    dispatchers: dict[tuple[int, int], DispatcherSnapshot]
    total_hw_irqs: int | None
    per_cpu_hw_irqs: tuple[int, ...]


async def _collect_driver_host_dispatchers(
    dut: Any,
) -> dict[tuple[int, int], DispatcherSnapshot]:
    """Queries `fuchsia.driver.development.Manager/GetDriverHostInfo`."""
    manager_proxy = f_driver_development.ManagerClient(
        dut.fuchsia_controller.connect_device_proxy(
            _DRIVER_DEVELOPMENT_ENDPOINT
        )
    )
    client, server = dut.fuchsia_controller.channel_create()
    iterator = f_driver_development.DriverHostInfoIteratorClient(client)
    manager_proxy.get_driver_host_info(iterator=server.take())

    dispatchers: dict[tuple[int, int], DispatcherSnapshot] = {}
    while True:
        resp = await iterator.get_next()
        hosts = resp.driver_hosts or []
        if not hosts:
            break
        for host in hosts:
            process_koid = int(host.process_koid or 0)
            host_name = str(host.name or "driver_host")
            for disp in host.dispatchers or []:
                if disp.debug_stats is None:
                    continue
                if disp.debug_stats.num_total_requests is None:
                    continue
                dispatcher_ptr = int(disp.dispatcher_ptr or 0)
                snap = DispatcherSnapshot(
                    driver_host_name=host_name,
                    process_koid=process_koid,
                    driver_url=str(disp.driver or "<unknown-driver>"),
                    dispatcher_name=str(disp.name or "<unnamed-dispatcher>"),
                    dispatcher_ptr=dispatcher_ptr,
                    num_total_requests=int(disp.debug_stats.num_total_requests),
                )
                dispatchers[snap.key] = snap
    return dispatchers


async def _collect_kernel_hw_irqs(
    dut: Any,
) -> tuple[int | None, tuple[int, ...]]:
    """Queries `fuchsia.kernel.Stats/GetCpuStats` for hardware interrupt counts."""
    rcs_proxy = fd_remotecontrol.RemoteControlClient(
        dut.fuchsia_controller.ctx.connect_remote_control_proxy()
    )
    client, server = dut.fuchsia_controller.channel_create()
    (
        await rcs_proxy.connect_capability(
            moniker=_KERNEL_STATS_MONIKER,
            capability_set=f_sys2.OpenDirType.NAMESPACE_DIR,
            capability_name=_KERNEL_STATS_CAPABILITY_NAME,
            server_channel=server.take(),
        )
    ).unwrap()
    stats_proxy = f_kernel.StatsClient(client)
    resp = await stats_proxy.get_cpu_stats()
    per_cpu = resp.stats.per_cpu_stats or []
    per_cpu_ints = tuple(int(cpu.ints or 0) for cpu in per_cpu)
    if not per_cpu_ints:
        return None, ()
    return sum(per_cpu_ints), per_cpu_ints


async def take_irq_storm_snapshot(dut: Any) -> IrqStormSnapshot:
    """Captures driver dispatcher counters and kernel IRQ stats.

    Args:
        dut: Honeydew FuchsiaDevice (must be online).

    Returns:
        An `IrqStormSnapshot` with current counters and monotonic timestamp.
    """
    timestamp = time.monotonic()
    dispatchers = await _collect_driver_host_dispatchers(dut)
    total_hw_irqs, per_cpu_hw_irqs = await _collect_kernel_hw_irqs(dut)

    return IrqStormSnapshot(
        timestamp_monotonic_sec=timestamp,
        dispatchers=dispatchers,
        total_hw_irqs=total_hw_irqs,
        per_cpu_hw_irqs=per_cpu_hw_irqs,
    )


def assert_no_irq_storm(
    before: IrqStormSnapshot,
    after: IrqStormSnapshot,
    *,
    phase: str = "",
    max_dispatcher_request_rate_hz: float = DEFAULT_MAX_DISPATCHER_REQUEST_RATE_HZ,
    min_dispatcher_request_delta: int = DEFAULT_MIN_DISPATCHER_REQUEST_DELTA,
    max_hw_irq_rate_hz: float = DEFAULT_MAX_HW_IRQ_RATE_HZ,
    min_hw_irq_delta: int = DEFAULT_MIN_HW_IRQ_DELTA,
) -> None:
    """Asserts that no driver spin-loop or unhandled IRQ storm occurred.

    Compares two `IrqStormSnapshot` instances captured before and after a test
    operation (such as a USB disconnect/reconnect cycle) and fails the test with
    a diagnostic summary if any layer is empty or exceeds its threshold.

    Args:
        before: Snapshot taken before the test action.
        after: Snapshot taken after the test action.
        phase: Optional label for log and failure messages, e.g. "iteration 1".
        max_dispatcher_request_rate_hz: Maximum allowed per-dispatcher request
            rate in requests/second.
        min_dispatcher_request_delta: Minimum request delta required before
            flagging a dispatcher rate violation (avoids false positives on
            very short windows).
        max_hw_irq_rate_hz: Maximum allowed system-wide hardware interrupt rate
            in interrupts/second.
        min_hw_irq_delta: Minimum total hardware interrupt delta required before
            flagging a kernel IRQ rate violation.
    """
    elapsed_sec = max(
        after.timestamp_monotonic_sec - before.timestamp_monotonic_sec, 1e-6
    )
    prefix = f"[{phase}] " if phase else ""
    violations: list[str] = []

    if not before.dispatchers or not after.dispatchers:
        violations.append(
            "No Driver Runtime dispatcher metrics collected in snapshot(s) "
            f"(before={len(before.dispatchers)}, after={len(after.dispatchers)})"
        )
    if (
        before.total_hw_irqs is None
        or after.total_hw_irqs is None
        or not before.per_cpu_hw_irqs
        or not after.per_cpu_hw_irqs
    ):
        violations.append(
            "No kernel per-CPU hardware IRQ stats collected in snapshot(s) "
            f"(before={len(before.per_cpu_hw_irqs)}, after={len(after.per_cpu_hw_irqs)})"
        )

    # 1. Check Driver Runtime per-dispatcher request rates across all drivers.
    peak_disp: DispatcherSnapshot | None = None
    peak_disp_delta: int = 0
    peak_disp_rate_hz: float = 0.0

    for key, after_disp in after.dispatchers.items():
        before_disp = before.dispatchers.get(key)
        if (
            before_disp is None
            or after_disp.num_total_requests < before_disp.num_total_requests
            or after_disp.driver_url != before_disp.driver_url
            or after_disp.dispatcher_name != before_disp.dispatcher_name
        ):
            # Created during the window (possibly reusing a destroyed
            # dispatcher's address), so its counter started at 0.
            delta = after_disp.num_total_requests
        else:
            delta = (
                after_disp.num_total_requests - before_disp.num_total_requests
            )
        rate_hz = delta / elapsed_sec
        if rate_hz > peak_disp_rate_hz:
            peak_disp = after_disp
            peak_disp_delta = delta
            peak_disp_rate_hz = rate_hz

        if (
            delta >= min_dispatcher_request_delta
            and rate_hz > max_dispatcher_request_rate_hz
        ):
            violations.append(
                f"Driver dispatcher spin-loop / storm on "
                f"driver={after_disp.driver_url!r} "
                f"dispatcher={after_disp.dispatcher_name!r} "
                f"(host={after_disp.driver_host_name!r}, "
                f"koid={after_disp.process_koid}): "
                f"+{delta} requests in {elapsed_sec:.2f}s "
                f"({rate_hz:.1f} req/s > {max_dispatcher_request_rate_hz:.1f} req/s)"
            )

    # 2. Check kernel hardware interrupt rate across all CPUs.
    hw_irq_delta: int | None = None
    hw_irq_rate_hz: float | None = None
    if before.total_hw_irqs is not None and after.total_hw_irqs is not None:
        hw_irq_delta = after.total_hw_irqs - before.total_hw_irqs
        if hw_irq_delta < 0:
            violations.append(
                f"Kernel hardware IRQ counter decreased across window "
                f"(possible unexpected reboot): before={before.total_hw_irqs}, "
                f"after={after.total_hw_irqs} (delta={hw_irq_delta})"
            )
        else:
            hw_irq_rate_hz = hw_irq_delta / elapsed_sec
            if (
                hw_irq_delta >= min_hw_irq_delta
                and hw_irq_rate_hz > max_hw_irq_rate_hz
            ):
                per_cpu_deltas = [
                    a - b
                    for b, a in zip(
                        before.per_cpu_hw_irqs, after.per_cpu_hw_irqs
                    )
                ]
                violations.append(
                    f"Kernel hardware IRQ storm: +{hw_irq_delta} hw IRQs in "
                    f"{elapsed_sec:.2f}s ({hw_irq_rate_hz:.1f} irq/s > "
                    f"{max_hw_irq_rate_hz:.1f} irq/s, "
                    f"per_cpu_deltas={per_cpu_deltas})"
                )

    peak_disp_summary = (
        f"{peak_disp.driver_url}:{peak_disp.dispatcher_name} "
        f"+{peak_disp_delta} ({peak_disp_rate_hz:.1f} req/s)"
        if peak_disp is not None
        else "n/a"
    )
    hw_irq_summary = (
        f"+{hw_irq_delta} ({hw_irq_rate_hz:.1f} irq/s)"
        if hw_irq_delta is not None and hw_irq_rate_hz is not None
        else "n/a"
    )

    _LOGGER.info(
        "%sIRQ/dispatcher health over %.2fs: peak_dispatcher=[%s], "
        "kernel_hw_irqs=[%s]",
        prefix,
        elapsed_sec,
        peak_disp_summary,
        hw_irq_summary,
    )

    if violations:
        asserts.fail(
            f"{prefix}Detected IRQ storm / driver spin-loop:\n  - "
            + "\n  - ".join(violations)
        )
