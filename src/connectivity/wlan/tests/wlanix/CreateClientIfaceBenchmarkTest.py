# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""
Benchmark test that creates and destroys a wlanix client interface repeatedly
to measure the time required for interface creation.
"""
import json
import logging
import os
import statistics

import fuchsia_wlan_base_test
import perf_publish.publish as publish
import test_data
import wlanix_testing
from mobly import asserts, test_runner
from trace_processing import trace_importing, trace_model, trace_utils

logger = logging.getLogger(__name__)

NUM_ITERATIONS = 20


class CreateClientIfaceBenchmarkTest(
    fuchsia_wlan_base_test.FuchsiaWlanBaseTest
):
    wlanix: wlanix_testing.Wlanix
    chip: wlanix_testing.WifiChip

    async def setup_class(self) -> None:
        await super().setup_class()
        self.wlanix = wlanix_testing.Wlanix(self.dut)
        self.chip = await self.wlanix.ensure_single_chip()

    async def setup_test(self) -> None:
        await super().setup_test()
        await self.chip.destroy_all_sta_ifaces()
        self.trace_file = "benchmark.fxt"
        self.trace_path = os.path.join(self.log_path, self.trace_file)

    async def teardown_test(self) -> None:
        await self.chip.destroy_all_sta_ifaces()
        if os.path.exists(self.trace_path):
            logger.info(f"Trace file generated at: {self.trace_path}")
        await super().teardown_test()

    def _log_summary_stats(
        self, durations_ms: list[float], metric_name: str
    ) -> None:
        if not durations_ms:
            logger.warning(f"No timing data collected for {metric_name}!")
            return

        avg_duration = statistics.mean(durations_ms)
        min_duration = min(durations_ms)
        max_duration = max(durations_ms)
        std_dev = (
            statistics.stdev(durations_ms) if len(durations_ms) > 1 else 0.0
        )

        logger.info(f"--- {metric_name} Performance Metrics ---")
        logger.info(f"Iterations: {len(durations_ms)}")
        logger.info(f"Average:    {avg_duration:.2f} ms")
        logger.info(f"Minimum:    {min_duration:.2f} ms")
        logger.info(f"Maximum:    {max_duration:.2f} ms")
        logger.info(f"Std Dev:    {std_dev:.2f} ms")
        logger.info("----------------------------------------------")

    def _get_event_durations(
        self, model: trace_model.Model, event_name: str
    ) -> list[float]:
        events = list(
            trace_utils.filter_events(
                model.all_events(),
                category="wlan",
                name=event_name,
                type=trace_model.DurationEvent,
            )
        )
        if len(events) < NUM_ITERATIONS:
            logger.warning(
                f"Expected {NUM_ITERATIONS} '{event_name}' events but only found {len(events)}."
            )

        durations_ms = []
        for event in events:
            if event.duration is not None:
                duration_ms = event.duration.to_microseconds_f() / 1000.0
                durations_ms.append(duration_ms)
            else:
                logger.warning(f"Found '{event_name}' event with no duration")
        return durations_ms

    async def test_create_destroy_client_iface(self) -> None:
        async with self.dut.tracing.trace_session(
            categories=["wlan"],
            download=True,
            directory=self.log_path,
            trace_file=self.trace_file,
        ):
            for i in range(NUM_ITERATIONS):
                logger.info(f"Iteration {i+1}: Creating client iface")
                iface = await self.chip.create_sta_iface(
                    attach_supplicant=False
                )
                logger.info(
                    f"Iteration {i+1}: Created iface {iface.name}. Now destroying."
                )

                await iface.remove()

                logger.info(
                    f"Iteration {i+1}: Successfully destroyed iface {iface.name}"
                )

        model = trace_importing.create_model_from_trace_file_path(
            self.trace_path,
            patterns=set(["create_client_iface", "destroy_client_iface"]),
        )

        creation_durations_ms = self._get_event_durations(
            model, "create_client_iface"
        )
        destruction_durations_ms = self._get_event_durations(
            model, "destroy_client_iface"
        )

        if creation_durations_ms and destruction_durations_ms:
            # Publish fuchsiaperf data extracted from our trace file.
            fuchsiaperf_data = [
                {
                    "test_suite": "fuchsia.wlan.wlanix",
                    "label": "CreateClientIface",
                    "values": creation_durations_ms,
                    "unit": "ms",
                },
                {
                    "test_suite": "fuchsia.wlan.wlanix",
                    "label": "DestroyClientIface",
                    "values": destruction_durations_ms,
                    "unit": "ms",
                },
            ]
            test_perf_file = os.path.join(
                self.log_path, "test.fuchsiaperf.json"
            )
            with open(test_perf_file, "w") as f:
                json.dump(fuchsiaperf_data, f)

            print("Publishing fuchsiaperf data to fuchsia-perf")
            publish.publish_fuchsiaperf(
                [test_perf_file],
                "fuchsia.wlan.wlanix.txt",
                test_data_module=test_data,
                runtime_deps_dir=".",
            )

            # Log summary statistics for our current run.
            self._log_summary_stats(creation_durations_ms, "Interface Creation")
            self._log_summary_stats(
                destruction_durations_ms, "Interface Destruction"
            )
        else:
            logger.error("No timing data collected from trace!")
            asserts.fail("No timing data collected")


if __name__ == "__main__":
    test_runner.main()
