#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""E2E test for diagnostics functionality.

Test that asserts that we can read logs and Inspect.
"""

import json
import logging
from typing import Any, Dict, List

import fuchsia_base_test
from mobly import asserts, test_runner
from perf import action_timer

_LOGGER: logging.Logger = logging.getLogger(__name__)
_TEST_SUITE = "fuchsia.test.diagnostics"


class DiagnosticsTest(fuchsia_base_test.FuchsiaBaseTest):
    async def setup_class(self) -> None:
        await super().setup_class()
        self._repeat_count: int = self.user_params["repeat_count"]

    def test_inspect(self) -> None:
        """Validates that we can snapshot Inspect from the device."""
        with action_timer.timer(
            _TEST_SUITE, "Inspect", self.test_case_path
        ) as t:
            for _ in range(self._repeat_count):
                with t.record_iteration():
                    result = self.dut.ffx.run(
                        cmd=["--machine", "json", "inspect", "show"],
                        log_output=False,
                    )
                inspect_data: list[Any] = json.loads(result)
                asserts.assert_greater(len(inspect_data), 0)
                self._check_archivist_data(inspect_data)
                self._check_component_manager_data(inspect_data)
                self._check_kernel_debug_broker_data(inspect_data)
                self._check_drivers_data(inspect_data)
                self._check_netstack_data(inspect_data)
                self._check_persistence_data(inspect_data)
                self._check_fshost_data(inspect_data)

    def _get_single_payload(
        self, inspect_data: List[Dict[str, Any]], moniker: str
    ) -> Dict[str, Any]:
        """Finds the Inspect payload for a moniker, asserting exactly one match exists."""
        matching: List[Dict[str, Any]] = [
            data for data in inspect_data if data.get("moniker") == moniker
        ]
        asserts.assert_equal(
            len(matching),
            1,
            f"Expected to find exactly one entry for moniker '{moniker}' in Inspect output, found {len(matching)}.",
        )
        payload = matching[0].get("payload")
        asserts.assert_is_not_none(
            payload,
            f"Expected non-null payload for moniker '{moniker}'.",
        )
        assert payload is not None
        root = payload.get("root")
        asserts.assert_is_not_none(
            root,
            f"Expected 'root' in payload for moniker '{moniker}'.",
        )
        assert isinstance(root, dict)
        return root

    def _find_payload(
        self, inspect_data: List[Dict[str, Any]], moniker: str
    ) -> Dict[str, Any] | None:
        """Finds the Inspect root payload for a moniker if present."""
        matching = [
            data for data in inspect_data if data.get("moniker") == moniker
        ]
        if not matching:
            return None
        asserts.assert_equal(
            len(matching),
            1,
            f"Expected at most one entry for moniker '{moniker}', found {len(matching)}.",
        )
        payload = matching[0].get("payload")
        asserts.assert_is_not_none(
            payload, f"Expected non-null payload for moniker '{moniker}'."
        )
        assert payload is not None
        root = payload.get("root")
        asserts.assert_is_not_none(
            root, f"Expected 'root' in payload for moniker '{moniker}'."
        )
        assert isinstance(root, dict)
        return root

    def _check_archivist_data(self, inspect_data: List[Dict[str, Any]]) -> None:
        """Verify Archivist Inspect data and stats."""
        root = self._get_single_payload(inspect_data, "bootstrap/archivist")
        health = root.get("fuchsia.inspect.Health", {})
        asserts.assert_equal(
            health.get("status"),
            "OK",
            "Archivist did not return OK health status",
        )
        asserts.assert_in(
            "archive_accessor_stats",
            root,
            "Archivist root does not contain archive_accessor_stats",
        )
        accessor_stats = root["archive_accessor_stats"]
        asserts.assert_in(
            "all",
            accessor_stats,
            "archive_accessor_stats does not contain 'all'",
        )
        all_stats = accessor_stats["all"]
        asserts.assert_in(
            "connections_opened",
            all_stats,
            "archive_accessor_stats/all missing connections_opened",
        )
        asserts.assert_in(
            "inspect",
            all_stats,
            "archive_accessor_stats/all missing inspect",
        )
        asserts.assert_in(
            "logs",
            all_stats,
            "archive_accessor_stats/all missing logs",
        )

    def _check_component_manager_data(
        self, inspect_data: List[Dict[str, Any]]
    ) -> None:
        """Verify Component Manager Inspect health, buffer size limits, and task stats."""
        root = self._get_single_payload(inspect_data, "<component_manager>")
        health = root.get("fuchsia.inspect.Health", {})
        asserts.assert_equal(
            health.get("status"),
            "OK",
            "Component manager did not return OK health status",
        )
        asserts.assert_in(
            "fuchsia.inspect.Stats",
            root,
            "Component manager missing fuchsia.inspect.Stats",
        )
        stats = root["fuchsia.inspect.Stats"]
        current_size = stats.get("current_size", 0)
        maximum_size = stats.get("maximum_size", 0)
        asserts.assert_less(
            current_size,
            350 * 1024,
            f"Component manager current_size {current_size} exceeds 350 KiB",
        )
        asserts.assert_greater_equal(
            maximum_size,
            350 * 1024,
            f"Component manager maximum_size {maximum_size} less than 350 KiB",
        )

        asserts.assert_in(
            "stats", root, "Component manager missing 'stats' node"
        )
        cm_stats = root["stats"]
        asserts.assert_in(
            "measurements",
            cm_stats,
            "Component manager missing stats/measurements",
        )
        measurements = cm_stats["measurements"]
        asserts.assert_greater(
            measurements.get("task_count", 0),
            0,
            "Component manager stats/measurements:task_count should be > 0",
        )
        asserts.assert_in(
            "components",
            measurements,
            "Component manager missing stats/measurements/components",
        )
        components = measurements["components"]
        for component_name in ["<component_manager>", "bootstrap/archivist"]:
            asserts.assert_in(
                component_name,
                components,
                f"stats/measurements/components missing '{component_name}'",
            )
            tasks = components[component_name]
            asserts.assert_greater(
                len(tasks),
                0,
                f"stats/measurements/components missing tasks for '{component_name}'",
            )
            for task_id, task_data in tasks.items():
                for metric in ["cpu_times", "queue_times", "timestamps"]:
                    asserts.assert_in(
                        metric,
                        task_data,
                        f"Component '{component_name}' task '{task_id}' missing measurement '{metric}'",
                    )
                    asserts.assert_greater(
                        len(task_data[metric]),
                        0,
                        f"Component '{component_name}' task '{task_id}' measurement '{metric}' is empty",
                    )
        asserts.assert_in(
            "recent_usage",
            cm_stats,
            "Component manager missing stats/recent_usage",
        )

    def _check_kernel_debug_broker_data(
        self, inspect_data: List[Dict[str, Any]]
    ) -> None:
        """Verify kernel debug broker Inspect data contains handles."""
        root = self._get_single_payload(
            inspect_data, "bootstrap/kernel_debug_broker"
        )
        asserts.assert_in(
            "handles",
            root,
            "kernel_debug_broker root inspect does not contain handles",
        )

    def _check_drivers_data(self, inspect_data: List[Dict[str, Any]]) -> None:
        """Verify boot drivers Inspect data."""
        driver_entries = [
            data
            for data in inspect_data
            if data.get("moniker", "").startswith("bootstrap/boot-drivers")
        ]
        asserts.assert_greater(
            len(driver_entries),
            0,
            "Expected to find at least one 'bootstrap/boot-drivers' component in Inspect output.",
        )
        for driver in driver_entries:
            payload = driver.get("payload")
            asserts.assert_is_not_none(
                payload,
                f"Boot driver {driver.get('moniker')} has null payload",
            )
            assert payload is not None
            asserts.assert_in(
                "root",
                payload,
                f"Boot driver {driver.get('moniker')} payload missing 'root'",
            )

    def _check_netstack_data(self, inspect_data: List[Dict[str, Any]]) -> None:
        """Verify Netstack health Inspect data if present."""
        root = self._find_payload(inspect_data, "core/network/netstack")
        if root is not None:
            health = root.get("fuchsia.inspect.Health", {})
            asserts.assert_equal(
                health.get("status"),
                "OK",
                "Netstack did not return OK health status",
            )

    def _check_persistence_data(
        self, inspect_data: List[Dict[str, Any]]
    ) -> None:
        """Verify persistence health Inspect data if present."""
        root = self._find_payload(inspect_data, "core/diagnostics/persistence")
        if root is not None:
            health = root.get("fuchsia.inspect.Health", {})
            status = health.get("status")
            asserts.assert_in(
                status,
                ["OK", "STARTING_UP"],
                f"Unexpected persistence health status: {status}",
            )

    def _check_fshost_data(self, inspect_data: List[Dict[str, Any]]) -> None:
        """Verify fshost filesystem stats."""
        root = self._get_single_payload(inspect_data, "bootstrap/fshost")
        asserts.assert_in(
            "data_stats",
            root,
            "bootstrap/fshost missing 'data_stats' in Inspect output",
        )
        data_stats = root["data_stats"]
        asserts.assert_in(
            "stats",
            data_stats,
            "bootstrap/fshost data_stats missing 'stats'",
        )
        stats = data_stats["stats"]
        asserts.assert_greater(stats.get("total_bytes", 0), 0)
        asserts.assert_greater(stats.get("allocated_bytes", 0), 0)
        asserts.assert_greater_equal(stats.get("used_bytes", -1), 0)
        asserts.assert_greater(stats.get("allocated_inodes", 0), 0)
        asserts.assert_greater_equal(stats.get("used_inodes", -1), 0)

    def test_logs(self) -> None:
        """Validates that we can snapshot logs from the device."""
        with action_timer.timer(_TEST_SUITE, "Logs", self.test_case_path) as t:
            for _ in range(self._repeat_count):
                with t.record_iteration():
                    logger_output = self.dut.ffx.run(
                        cmd=[
                            "--machine",
                            "json",
                            "log",
                            "--symbolize",
                            "off",
                            "dump",
                        ],
                        log_output=False,
                    )
                asserts.assert_greater(len(logger_output), 0)


if __name__ == "__main__":
    test_runner.main()
