# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Host unit tests for irq_storm detection utilities."""

import dataclasses
import types
import unittest
from unittest import mock

from mobly import signals
from usb_lib.irq_storm import (
    DispatcherSnapshot,
    IrqStormSnapshot,
    assert_no_irq_storm,
    take_irq_storm_snapshot,
)


class IrqStormSnapshotCollectionTest(unittest.IsolatedAsyncioTestCase):
    """Tests `take_irq_storm_snapshot` FIDL data collection."""

    @mock.patch("usb_lib.irq_storm.f_kernel.StatsClient")
    @mock.patch("usb_lib.irq_storm.fd_remotecontrol.RemoteControlClient")
    @mock.patch(
        "usb_lib.irq_storm.f_driver_development.DriverHostInfoIteratorClient"
    )
    @mock.patch("usb_lib.irq_storm.f_driver_development.ManagerClient")
    async def test_take_irq_storm_snapshot_collects_all_sources(
        self,
        mock_manager_client_cls: mock.MagicMock,
        mock_iterator_client_cls: mock.MagicMock,
        mock_rcs_client_cls: mock.MagicMock,
        mock_stats_client_cls: mock.MagicMock,
    ) -> None:
        dut = mock.MagicMock()
        dut.device_name = "fuchsia-dut"
        ch_client = mock.MagicMock()
        ch_server = mock.MagicMock()
        ch_server.take.return_value = 42
        dut.fuchsia_controller.channel_create.return_value = (
            ch_client,
            ch_server,
        )

        # Mock DriverHostInfoIterator returning one page then empty page.
        disp_info = types.SimpleNamespace(
            driver="fuchsia-boot:///dwc3#meta/dwc3.cm",
            name="dwc3-irq",
            dispatcher_ptr=0x1234,
            debug_stats=types.SimpleNamespace(num_total_requests=150),
        )
        host_info = types.SimpleNamespace(
            process_koid=999,
            name="driver-host-usb",
            dispatchers=[disp_info],
        )
        mock_iterator = mock.MagicMock()
        mock_iterator.get_next = mock.AsyncMock(
            side_effect=[
                types.SimpleNamespace(driver_hosts=[host_info]),
                types.SimpleNamespace(driver_hosts=[]),
            ]
        )
        mock_iterator_client_cls.return_value = mock_iterator

        # Mock RemoteControl + Kernel Stats.
        mock_rcs = mock.MagicMock()
        connect_res = mock.MagicMock()
        mock_rcs.connect_capability = mock.AsyncMock(return_value=connect_res)
        mock_rcs_client_cls.return_value = mock_rcs

        mock_stats = mock.MagicMock()
        mock_stats.get_cpu_stats = mock.AsyncMock(
            return_value=types.SimpleNamespace(
                stats=types.SimpleNamespace(
                    per_cpu_stats=[
                        types.SimpleNamespace(ints=1000),
                        types.SimpleNamespace(ints=2500),
                    ]
                )
            )
        )
        mock_stats_client_cls.return_value = mock_stats

        snap = await take_irq_storm_snapshot(dut)

        mock_manager_client_cls.return_value.get_driver_host_info.assert_called_once_with(
            iterator=42
        )
        self.assertIn((999, 0x1234), snap.dispatchers)
        disp_snap = snap.dispatchers[(999, 0x1234)]
        self.assertEqual(
            disp_snap.driver_url, "fuchsia-boot:///dwc3#meta/dwc3.cm"
        )
        self.assertEqual(disp_snap.dispatcher_name, "dwc3-irq")
        self.assertEqual(disp_snap.num_total_requests, 150)

        self.assertEqual(snap.total_hw_irqs, 3500)
        self.assertEqual(snap.per_cpu_hw_irqs, (1000, 2500))

    async def test_take_irq_storm_snapshot_propagates_collector_errors(
        self,
    ) -> None:
        dut = mock.MagicMock()
        dut.device_name = "fuchsia-dut"
        dut.fuchsia_controller.connect_device_proxy.side_effect = RuntimeError(
            "proxy failed"
        )

        with self.assertRaises(RuntimeError):
            await take_irq_storm_snapshot(dut)


class AssertNoIrqStormTest(unittest.TestCase):
    """Tests `assert_no_irq_storm` detection across both system layers."""

    def _make_snapshot(
        self,
        *,
        timestamp: float,
        disp_requests: int = 100,
        total_hw_irqs: int = 1000,
        per_cpu_hw_irqs: tuple[int, ...] = (500, 500),
    ) -> IrqStormSnapshot:
        disp = DispatcherSnapshot(
            driver_host_name="driver-host-usb",
            process_koid=101,
            driver_url="fuchsia-boot:///dwc3#meta/dwc3.cm",
            dispatcher_name="dwc3-default",
            dispatcher_ptr=0xABCD,
            num_total_requests=disp_requests,
        )
        return IrqStormSnapshot(
            timestamp_monotonic_sec=timestamp,
            dispatchers={disp.key: disp},
            total_hw_irqs=total_hw_irqs,
            per_cpu_hw_irqs=per_cpu_hw_irqs,
        )

    def test_healthy_delta_passes(self) -> None:
        before = self._make_snapshot(
            timestamp=10.0,
            disp_requests=100,
            total_hw_irqs=1000,
            per_cpu_hw_irqs=(500, 500),
        )
        after = self._make_snapshot(
            timestamp=20.0,
            disp_requests=350,
            total_hw_irqs=1400,
            per_cpu_hw_irqs=(700, 700),
        )
        assert_no_irq_storm(before, after, phase="iteration 1")

    def test_empty_dispatchers_fails(self) -> None:
        before = self._make_snapshot(timestamp=10.0)
        after = IrqStormSnapshot(
            timestamp_monotonic_sec=20.0,
            dispatchers={},
            total_hw_irqs=1400,
            per_cpu_hw_irqs=(700, 700),
        )
        with self.assertRaises(signals.TestFailure) as ctx:
            assert_no_irq_storm(before, after, phase="iteration 1")
        self.assertIn(
            "No Driver Runtime dispatcher metrics collected",
            str(ctx.exception),
        )

    def test_empty_cpu_stats_fails(self) -> None:
        before = self._make_snapshot(timestamp=10.0)
        after = IrqStormSnapshot(
            timestamp_monotonic_sec=20.0,
            dispatchers=before.dispatchers,
            total_hw_irqs=None,
            per_cpu_hw_irqs=(),
        )
        with self.assertRaises(signals.TestFailure) as ctx:
            assert_no_irq_storm(before, after, phase="iteration 1")
        self.assertIn(
            "No kernel per-CPU hardware IRQ stats collected",
            str(ctx.exception),
        )

    def test_dispatcher_spin_loop_fails(self) -> None:
        before = self._make_snapshot(timestamp=10.0, disp_requests=100)
        # +100,000 requests in 10 seconds = 10,000 req/s (> 5,000 req/s)
        after = self._make_snapshot(timestamp=20.0, disp_requests=100100)
        with self.assertRaises(signals.TestFailure) as ctx:
            assert_no_irq_storm(before, after, phase="iteration 2")
        self.assertIn("Driver dispatcher spin-loop / storm", str(ctx.exception))
        self.assertIn("fuchsia-boot:///dwc3#meta/dwc3.cm", str(ctx.exception))
        self.assertIn("iteration 2", str(ctx.exception))

    def test_newly_spawned_dispatcher_spin_loop_fails(self) -> None:
        before = self._make_snapshot(timestamp=10.0, disp_requests=100)
        after = self._make_snapshot(timestamp=20.0, disp_requests=200)
        new_disp = DispatcherSnapshot(
            driver_host_name="driver-host-usb",
            process_koid=102,
            driver_url="fuchsia-boot:///usb-peripheral#meta/usb-peripheral.cm",
            dispatcher_name="usb-peripheral-worker",
            dispatcher_ptr=0xDEF0,
            num_total_requests=80000,
        )
        after.dispatchers[new_disp.key] = new_disp
        with self.assertRaises(signals.TestFailure) as ctx:
            assert_no_irq_storm(before, after, phase="iteration 3")
        self.assertIn("Driver dispatcher spin-loop / storm", str(ctx.exception))
        self.assertIn("usb-peripheral-worker", str(ctx.exception))

    def test_restarted_dispatcher_counter_spin_loop_fails(self) -> None:
        # A dispatcher re-created at the same address restarts its counter, so
        # a negative delta must not hide a spin-loop.
        before = self._make_snapshot(timestamp=10.0, disp_requests=500000)
        # +80,000 requests on the restarted counter in 10s = 8,000 req/s.
        after = self._make_snapshot(timestamp=20.0, disp_requests=80000)
        with self.assertRaises(signals.TestFailure) as ctx:
            assert_no_irq_storm(before, after, phase="iteration 5")
        self.assertIn("Driver dispatcher spin-loop / storm", str(ctx.exception))

    def test_dispatcher_address_reused_by_new_dispatcher_counts_from_zero(
        self,
    ) -> None:
        before = self._make_snapshot(timestamp=10.0, disp_requests=900000)
        old_disp = next(iter(before.dispatchers.values()))
        new_disp = dataclasses.replace(
            old_disp,
            driver_url="fuchsia-boot:///usb-peripheral#meta/usb-peripheral.cm",
            dispatcher_name="usb-peripheral-worker",
            num_total_requests=960000,
        )
        after = dataclasses.replace(
            self._make_snapshot(timestamp=30.0),
            dispatchers={new_disp.key: new_disp},
        )
        # A naive delta of +60,000 in 20s (3,000 req/s) would pass, but the new
        # dispatcher processed all 960,000 requests (48,000 req/s).
        with self.assertRaises(signals.TestFailure) as ctx:
            assert_no_irq_storm(before, after, phase="iteration 6")
        self.assertIn("usb-peripheral-worker", str(ctx.exception))

    def test_kernel_hw_irq_storm_fails(self) -> None:
        before = self._make_snapshot(
            timestamp=10.0,
            total_hw_irqs=1000,
            per_cpu_hw_irqs=(500, 500),
        )
        # +250,000 hw IRQs in 10s = 25,000 irq/s (> 10,000 irq/s)
        after = self._make_snapshot(
            timestamp=20.0,
            total_hw_irqs=251000,
            per_cpu_hw_irqs=(250500, 500),
        )
        with self.assertRaises(signals.TestFailure) as ctx:
            assert_no_irq_storm(before, after, phase="iteration 4")
        self.assertIn("Kernel hardware IRQ storm", str(ctx.exception))
        self.assertIn("per_cpu_deltas=[250000, 0]", str(ctx.exception))

    def test_kernel_hw_irq_counter_decreased_fails(self) -> None:
        before = self._make_snapshot(
            timestamp=10.0,
            total_hw_irqs=5000,
            per_cpu_hw_irqs=(2500, 2500),
        )
        after = self._make_snapshot(
            timestamp=20.0,
            total_hw_irqs=1000,
            per_cpu_hw_irqs=(500, 500),
        )
        with self.assertRaises(signals.TestFailure) as ctx:
            assert_no_irq_storm(before, after, phase="iteration 7")
        self.assertIn(
            "Kernel hardware IRQ counter decreased across window",
            str(ctx.exception),
        )


if __name__ == "__main__":
    unittest.main()
