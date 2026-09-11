# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Mobly E2E stress test suite for USB Zero Function (usb-zero-function).

Runs comprehensive stress test loops for a sustained duration across
high-throughput streaming, unaligned DMA, scatter/gather queues,
transfer cancellations, and toggle synchronization in Source/Sink mode.
"""

import logging

import zero_function
from mobly import test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)

_DEFAULT_STRESS_DURATION_SEC: float = 120.0  # 2 minutes per stress test


class ZeroFunctionStressTest(zero_function.ZeroFunctionBaseTest):
    """Stress test suite executing sustained loops across test categories."""

    def _get_stress_duration_sec(self) -> float:
        """Retrieve the configured stress duration per test."""
        return float(
            self.user_params.get(
                "stress_duration_sec", _DEFAULT_STRESS_DURATION_SEC
            )
        )

    def test_stress_high_throughput_streaming(self) -> None:
        """Stresses bulk pipelines with 31MB continuous streams."""
        dev_node = self.require_dev_node()
        duration = self._get_stress_duration_sec()
        _LOGGER.info(
            "Starting 31MB streaming stress loop on %s for %.1fs...",
            dev_node,
            duration,
        )

        self.execute_testusb_timed(
            dev_node=dev_node,
            test_ids=[27, 28],
            mode="sourcesink",
            duration_sec=duration,
            iterations_per_batch=1,
        )
        _LOGGER.info(
            "Successfully completed high-throughput bulk streaming stress loop."
        )

    def test_stress_unaligned_and_premapped_dma(self) -> None:
        """Stresses unaligned memory and DMA (Tests 17, 18, 19, 20)."""
        dev_node = self.require_dev_node()
        duration = self._get_stress_duration_sec()
        _LOGGER.info(
            "Starting unaligned & DMA stress loop on %s for %.1fs...",
            dev_node,
            duration,
        )

        self.execute_testusb_timed(
            dev_node=dev_node,
            test_ids=[17, 18, 19, 20],
            mode="sourcesink",
            duration_sec=duration,
            iterations_per_batch=50,
        )
        _LOGGER.info(
            "Successfully completed unaligned & pre-mapped DMA stress loop."
        )

    def test_stress_scatter_gather_queues(self) -> None:
        """Stresses scatter/gather queues (Tests 5, 6, 7, 8, 30, 31)."""
        dev_node = self.require_dev_node()
        duration = self._get_stress_duration_sec()
        _LOGGER.info(
            "Starting scatter/gather queue stress loop on %s for %.1fs...",
            dev_node,
            duration,
        )

        self.execute_testusb_timed(
            dev_node=dev_node,
            test_ids=[5, 6, 7, 8, 30, 31],
            mode="sourcesink",
            duration_sec=duration,
            iterations_per_batch=25,
        )
        _LOGGER.info("Successfully completed scatter/gather queue stress loop.")

    def test_stress_endpoint_halt_and_toggle_sync(self) -> None:
        """Stresses endpoint STALL state and toggle clearing (Tests 13 & 29)."""
        dev_node = self.require_dev_node()
        duration = self._get_stress_duration_sec()
        _LOGGER.info(
            "Starting endpoint halt & toggle stress loop on %s for %.1fs...",
            dev_node,
            duration,
        )

        self.execute_testusb_timed(
            dev_node=dev_node,
            test_ids=[13, 29],
            mode="sourcesink",
            duration_sec=duration,
            iterations_per_batch=5,
        )
        _LOGGER.info(
            "Successfully completed endpoint halt & toggle sync stress loop."
        )

    def test_stress_transfer_cancellation_unlink(self) -> None:
        """Stresses transfer cancellation and DMA aborts (Tests 11 & 12)."""
        dev_node = self.require_dev_node()
        duration = self._get_stress_duration_sec()
        _LOGGER.info(
            "Starting transfer cancellation stress loop on %s for %.1fs...",
            dev_node,
            duration,
        )

        self.execute_testusb_timed(
            dev_node=dev_node,
            test_ids=[11, 12],
            mode="sourcesink",
            duration_sec=duration,
            iterations_per_batch=20,
        )
        _LOGGER.info(
            "Successfully completed transfer cancellation stress loop."
        )

    # TODO(https://fxbug.dev/559943380): Re-enable once the target driver-host
    # crash/hang during rapid full-suite continuous sweeps is fixed.
    # def test_stress_full_suite_continuous_loop(self) -> None:
    #     """Sweeps all 27 supported tests continuously in a sustained loop."""
    #     dev_node = self.require_dev_node()
    #     duration = self._get_stress_duration_sec()
    #     _LOGGER.info(
    #         "Starting full test suite continuous loop on %s for %.1fs...",
    #         dev_node,
    #         duration,
    #     )
    #
    #     self.execute_testusb_timed(
    #         dev_node=dev_node,
    #         test_ids=zero_function.ALL_SUPPORTED_TEST_IDS,
    #         mode="sourcesink",
    #         duration_sec=duration,
    #         iterations_per_batch=1,
    #     )
    #     _LOGGER.info(
    #         "Successfully completed full test suite continuous stress loop."
    #     )


if __name__ == "__main__":
    test_runner.main()
