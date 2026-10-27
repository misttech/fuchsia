# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Host unit tests for DWC3 IRQ Inspect metrics collection and verification."""

import types
import unittest
from unittest import mock

from mobly import signals
from usb_lib.dwc3_irq_metrics import (
    Dwc3IrqSnapshot,
    assert_no_dwc3_unhandled_irq_storm,
    collect_dwc3_irq_metrics,
)
from usb_lib.irq_storm import DispatcherSnapshot, IrqStormSnapshot


class CollectDwc3IrqMetricsTest(unittest.TestCase):
    """Tests `collect_dwc3_irq_metrics` Inspect data collection."""

    def test_collect_dwc3_irq_metrics_parses_inspect_properties(self) -> None:
        dut = mock.MagicMock()
        inspect_entry = types.SimpleNamespace(
            moniker="bootstrap/boot-drivers:usb-0",
            payload={
                "root": {
                    "dwc3": {
                        "irq_count": 420,
                        "unhandled_irq_count": 0,
                    }
                }
            },
        )
        dut.get_inspect_data.return_value = types.SimpleNamespace(
            data=[inspect_entry]
        )

        metrics = collect_dwc3_irq_metrics(dut)

        self.assertIn("bootstrap/boot-drivers:usb-0", metrics)
        dwc3 = metrics["bootstrap/boot-drivers:usb-0"]
        self.assertEqual(dwc3.irq_count, 420)
        self.assertEqual(dwc3.unhandled_irq_count, 0)

    def test_collect_dwc3_irq_metrics_missing_properties_raises(self) -> None:
        dut = mock.MagicMock()
        dut.get_inspect_data.return_value = types.SimpleNamespace(
            data=[
                types.SimpleNamespace(
                    moniker="bootstrap/boot-drivers:usb-0",
                    payload={"root": {"dwc3": {"time_start": 123}}},
                )
            ]
        )

        with self.assertRaises(RuntimeError) as ctx:
            collect_dwc3_irq_metrics(dut)
        self.assertIn("missing irq_count", str(ctx.exception))


class AssertNoDwc3UnhandledIrqStormTest(unittest.TestCase):
    """Tests `assert_no_dwc3_unhandled_irq_storm` detection."""

    def _make_irq_snapshot(
        self,
        *,
        timestamp: float,
        driver_url: str = "fuchsia-boot:///dwc3#meta/dwc3.cm",
    ) -> IrqStormSnapshot:
        disp = DispatcherSnapshot(
            driver_host_name="driver-host-usb",
            process_koid=101,
            driver_url=driver_url,
            dispatcher_name="dwc3-default",
            dispatcher_ptr=0xABCD,
            num_total_requests=100,
        )
        return IrqStormSnapshot(
            timestamp_monotonic_sec=timestamp,
            dispatchers={disp.key: disp},
            total_hw_irqs=1000,
            per_cpu_hw_irqs=(500, 500),
        )

    def _make_dwc3_metrics(
        self,
        *,
        irq_count: int = 50,
        unhandled_irq_count: int = 0,
    ) -> dict[str, Dwc3IrqSnapshot]:
        snap = Dwc3IrqSnapshot(
            moniker="bootstrap/boot-drivers:usb-0",
            irq_count=irq_count,
            unhandled_irq_count=unhandled_irq_count,
        )
        return {snap.moniker: snap}

    def test_healthy_dwc3_delta_passes(self) -> None:
        before_irq = self._make_irq_snapshot(timestamp=10.0)
        after_irq = self._make_irq_snapshot(timestamp=20.0)
        before = self._make_dwc3_metrics(irq_count=50, unhandled_irq_count=0)
        after = self._make_dwc3_metrics(irq_count=200, unhandled_irq_count=1)
        assert_no_dwc3_unhandled_irq_storm(
            before,
            after,
            before_irq_snapshot=before_irq,
            after_irq_snapshot=after_irq,
            phase="iteration 1",
        )

    def test_dwc3_driver_loaded_without_inspect_metrics_fails(self) -> None:
        before_irq = self._make_irq_snapshot(timestamp=10.0)
        after_irq = self._make_irq_snapshot(timestamp=20.0)
        before = self._make_dwc3_metrics(irq_count=50, unhandled_irq_count=0)
        with self.assertRaises(signals.TestFailure) as ctx:
            assert_no_dwc3_unhandled_irq_storm(
                before,
                {},
                before_irq_snapshot=before_irq,
                after_irq_snapshot=after_irq,
                phase="iteration 1",
            )
        self.assertIn(
            "DWC3 driver is loaded but no DWC3 Inspect metrics were collected "
            "in the after snapshot",
            str(ctx.exception),
        )

    def test_no_dwc3_driver_skips_dwc3_check(self) -> None:
        before_irq = self._make_irq_snapshot(
            timestamp=10.0,
            driver_url="fuchsia-boot:///dwc2#meta/dwc2.cm",
        )
        after_irq = self._make_irq_snapshot(
            timestamp=20.0,
            driver_url="fuchsia-boot:///dwc2#meta/dwc2.cm",
        )
        assert_no_dwc3_unhandled_irq_storm(
            {},
            {},
            before_irq_snapshot=before_irq,
            after_irq_snapshot=after_irq,
            phase="iteration 1",
        )

    def test_dwc3_unhandled_irq_storm_fails(self) -> None:
        before_irq = self._make_irq_snapshot(timestamp=10.0)
        after_irq = self._make_irq_snapshot(timestamp=20.0)
        before = self._make_dwc3_metrics(irq_count=50, unhandled_irq_count=2)
        after = self._make_dwc3_metrics(
            irq_count=5050, unhandled_irq_count=5002
        )
        with self.assertRaises(signals.TestFailure) as ctx:
            assert_no_dwc3_unhandled_irq_storm(
                before,
                after,
                before_irq_snapshot=before_irq,
                after_irq_snapshot=after_irq,
                phase="iteration 5",
            )
        self.assertIn("DWC3 unhandled IRQ storm", str(ctx.exception))
        self.assertIn("+5000 unhandled IRQs", str(ctx.exception))

    def test_restarted_dwc3_driver_unhandled_irq_storm_fails(self) -> None:
        # A DWC3 driver restart resets its counters, so a negative delta must
        # not hide an unhandled IRQ storm.
        before_irq = self._make_irq_snapshot(timestamp=10.0)
        after_irq = self._make_irq_snapshot(timestamp=20.0)
        before = self._make_dwc3_metrics(
            irq_count=10000, unhandled_irq_count=5000
        )
        after = self._make_dwc3_metrics(irq_count=600, unhandled_irq_count=500)
        with self.assertRaises(signals.TestFailure) as ctx:
            assert_no_dwc3_unhandled_irq_storm(
                before,
                after,
                before_irq_snapshot=before_irq,
                after_irq_snapshot=after_irq,
                phase="iteration 6",
            )
        self.assertIn("+500 unhandled IRQs", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
