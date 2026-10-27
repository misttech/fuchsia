# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Collects and verifies DWC3 USB controller IRQ Inspect metrics on a DUT.

Reads `root/dwc3:irq_count` and `root/dwc3:unhandled_irq_count` exposed by the
DWC3 driver in Inspect and asserts that `unhandled_irq_count` does not spike
across a test window (such as a USB disconnect/reconnect cycle).

Whenever a DWC3 driver is loaded in the Driver Runtime dispatcher snapshot,
DWC3 Inspect metrics must be present so that broken selectors or missing Inspect
properties fail loudly rather than silently skipping the check. Boards without a
DWC3 controller (e.g. dwc2 or xHCI) skip this check.
"""

import dataclasses
import logging
from typing import Any

from mobly import asserts

from .irq_storm import IrqStormSnapshot

_LOGGER: logging.Logger = logging.getLogger(__name__)

# DWC3 `unhandled_irq_count` threshold:
# - `Dwc3::HandleIrq` increments it for interrupts that arrive while the
#   controller is stopped, a disconnect drain is in flight, or the core is
#   powered off, or whose first `GEVNTCOUNT(0)` read reports no pending events.
# - Normal baseline is 0 across disconnect/reconnect cycles; 100 tolerates
#   isolated stop/reset transition races while catching even a 1-2ms burst of
#   unmasked interrupts that would average out over a 35s window.
DEFAULT_MAX_DWC3_UNHANDLED_IRQ_DELTA: int = 100

_DWC3_INSPECT_SELECTOR: str = r"bootstrap/*-drivers\:*:root/dwc3"
# Component URL suffix of the DWC3 driver, e.g.
# "fuchsia-boot:///dwc3#meta/dwc3.cm".
_DWC3_DRIVER_URL_SUFFIX: str = "#meta/dwc3.cm"


@dataclasses.dataclass(frozen=True)
class Dwc3IrqSnapshot:
    """Point-in-time DWC3 interrupt counters from Inspect."""

    moniker: str
    irq_count: int
    unhandled_irq_count: int


def collect_dwc3_irq_metrics(dut: Any) -> dict[str, Dwc3IrqSnapshot]:
    """Reads `root/dwc3` Inspect metrics (`irq_count`, `unhandled_irq_count`)."""
    metrics: dict[str, Dwc3IrqSnapshot] = {}
    inspect_collection = dut.get_inspect_data(
        selectors=[_DWC3_INSPECT_SELECTOR]
    )
    for data in inspect_collection.data:
        payload = data.payload or {}
        dwc3_node = payload.get("root", {}).get("dwc3")
        if not isinstance(dwc3_node, dict):
            continue
        moniker = str(getattr(data, "moniker", "dwc3"))
        if (
            "irq_count" not in dwc3_node
            or "unhandled_irq_count" not in dwc3_node
        ):
            raise RuntimeError(
                f"DWC3 Inspect node at {moniker} is missing irq_count or "
                f"unhandled_irq_count properties: {sorted(dwc3_node.keys())}"
            )
        metrics[moniker] = Dwc3IrqSnapshot(
            moniker=moniker,
            irq_count=int(dwc3_node["irq_count"]),
            unhandled_irq_count=int(dwc3_node["unhandled_irq_count"]),
        )
    return metrics


def _is_dwc3_driver_loaded(snapshot: IrqStormSnapshot) -> bool:
    """Returns whether any Driver Runtime dispatcher belongs to a DWC3 driver."""
    return any(
        disp.driver_url.endswith(_DWC3_DRIVER_URL_SUFFIX)
        for disp in snapshot.dispatchers.values()
    )


def assert_no_dwc3_unhandled_irq_storm(
    before: dict[str, Dwc3IrqSnapshot],
    after: dict[str, Dwc3IrqSnapshot],
    *,
    before_irq_snapshot: IrqStormSnapshot,
    after_irq_snapshot: IrqStormSnapshot,
    phase: str = "",
    max_dwc3_unhandled_irq_delta: int = DEFAULT_MAX_DWC3_UNHANDLED_IRQ_DELTA,
) -> None:
    """Asserts that DWC3 `unhandled_irq_count` did not storm across the window.

    Args:
        before: DWC3 Inspect metrics taken before the test action.
        after: DWC3 Inspect metrics taken after the test action.
        before_irq_snapshot: Generic `IrqStormSnapshot` taken before the action.
        after_irq_snapshot: Generic `IrqStormSnapshot` taken after the action.
        phase: Optional label for log and failure messages, e.g. "iteration 1".
        max_dwc3_unhandled_irq_delta: Maximum allowed increase in DWC3
            `unhandled_irq_count` across the window.
    """
    elapsed_sec = max(
        after_irq_snapshot.timestamp_monotonic_sec
        - before_irq_snapshot.timestamp_monotonic_sec,
        1e-6,
    )
    prefix = f"[{phase}] " if phase else ""
    violations: list[str] = []

    # DWC3 Inspect metrics are required whenever a DWC3 driver is loaded, so a
    # broken selector or missing properties fail loudly, while boards without
    # DWC3 (e.g. dwc2 or xHCI) skip this layer.
    for label, snap, dwc3_map in (
        ("before", before_irq_snapshot, before),
        ("after", after_irq_snapshot, after),
    ):
        if _is_dwc3_driver_loaded(snap) and not dwc3_map:
            violations.append(
                "DWC3 driver is loaded but no DWC3 Inspect metrics were "
                f"collected in the {label} snapshot"
            )

    dwc3_summaries: list[str] = []
    for moniker, after_dwc3 in after.items():
        before_dwc3 = before.get(moniker)
        if (
            before_dwc3 is None
            or after_dwc3.irq_count < before_dwc3.irq_count
            or after_dwc3.unhandled_irq_count < before_dwc3.unhandled_irq_count
        ):
            # Driver started or restarted during the window, so its counters
            # started at 0.
            irq_delta = after_dwc3.irq_count
            unhandled_delta = after_dwc3.unhandled_irq_count
        else:
            irq_delta = after_dwc3.irq_count - before_dwc3.irq_count
            unhandled_delta = (
                after_dwc3.unhandled_irq_count - before_dwc3.unhandled_irq_count
            )
        dwc3_summaries.append(
            f"{moniker}(irq=+{irq_delta}, unhandled=+{unhandled_delta})"
        )
        if unhandled_delta > max_dwc3_unhandled_irq_delta:
            violations.append(
                f"DWC3 unhandled IRQ storm on {moniker}: "
                f"+{unhandled_delta} unhandled IRQs (+{irq_delta} total IRQs) "
                f"in {elapsed_sec:.2f}s "
                f"(threshold={max_dwc3_unhandled_irq_delta})"
            )

    if dwc3_summaries:
        dwc3_summary = ", ".join(dwc3_summaries)
    elif not _is_dwc3_driver_loaded(
        before_irq_snapshot
    ) and not _is_dwc3_driver_loaded(after_irq_snapshot):
        dwc3_summary = "n/a (no DWC3 driver loaded)"
    else:
        dwc3_summary = "n/a"

    _LOGGER.info(
        "%sDWC3 IRQ health over %.2fs: dwc3=[%s]",
        prefix,
        elapsed_sec,
        dwc3_summary,
    )

    if violations:
        asserts.fail(
            f"{prefix}Detected DWC3 unhandled IRQ storm:\n  - "
            + "\n  - ".join(violations)
        )
