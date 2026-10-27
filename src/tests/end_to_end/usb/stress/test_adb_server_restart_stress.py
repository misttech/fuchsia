# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""ADB Server Restart (kill-server) Stress Test."""

import logging
import math
import time
from unittest import mock

import fuchsia_base_test
from honeydew.device_classes import android_device
from mobly import asserts, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)

# Default timeouts and thresholds in seconds.
_DEFAULT_RECOVERY_TIMEOUT_SEC: float = 90.0
_DEFAULT_MAX_ACCEPTABLE_RECOVERY_SEC: float = 60.0
_DEFAULT_CMD_TIMEOUT_SEC: float = 15.0
_DEFAULT_ROOT_TIMEOUT_SEC: float = 30.0


def _calculate_percentile(data: list[float], percentile: float) -> float:
    """Calculates the specified percentile from a list of floats.

    Args:
        data: List of numeric timing data.
        percentile: Float between 0.0 and 100.0.

    Returns:
        The calculated percentile value.
    """
    if not data:
        return 0.0
    sorted_data = sorted(data)
    if len(sorted_data) == 1:
        return sorted_data[0]
    rank = (percentile / 100.0) * (len(sorted_data) - 1)
    lower_idx = int(math.floor(rank))
    upper_idx = int(math.ceil(rank))
    weight = rank - lower_idx
    return (1.0 - weight) * sorted_data[lower_idx] + weight * sorted_data[
        upper_idx
    ]


class AdbServerRestartStressTest(fuchsia_base_test.FuchsiaBaseTest):
    """Stress test for restarting the host-side isolated ADB server.

    Repeatedly tears down and restarts the isolated ADB server (via
    kill-server and respawning), verifying device re-attachment, root state
    preservation, process isolation, and tracking recovery-time distribution.

    Required Mobly Test Params:
        num_iterations (int): Number of restart cycles to execute.

    Optional Mobly Test Params:
        recovery_timeout_sec (float): Hard timeout waiting for device
            re-attachment. Defaults to 90.0 seconds.
        max_acceptable_recovery_sec (float): Budget for recovery duration.
            Treats recovery taking longer as a reportable test failure.
            Defaults to 60.0 seconds.
    """

    dut: android_device.AndroidDevice

    async def pre_run(self) -> None:
        test_arg_tuple_list: list[tuple[int]] = []
        num_iterations = int(self.user_params.get("num_iterations", 10))
        for iteration in range(1, num_iterations + 1):
            test_arg_tuple_list.append((iteration,))

        self.generate_tests(
            test_logic=self._test_logic,
            name_func=self._name_func,
            arg_sets=test_arg_tuple_list,
        )

    async def setup_class(self) -> None:
        await super().setup_class()
        assert isinstance(self.dut, android_device.AndroidDevice)
        # Ensure ADB is supported and enabled on this device.
        assert hasattr(self.dut, "adb")

        # TODO(b/564946370): Remove this patch once AndroidDevice.adb's
        # temporary su_root monkey-patch is removed.
        for attr in ("root", "unroot", "use_adb_root"):
            orig_method = getattr(self.dut.adb.__class__, attr).__get__(
                self.dut.adb, self.dut.adb.__class__
            )
            mock.patch.object(self.dut.adb, attr, orig_method).start()

        # Verify that an isolated ADB server is active.
        assert (
            self.dut.adb._adb_server is not None
        ), "Isolated ADB server is required for this stress test"
        asserts.assert_is_not_none(
            self.dut.adb._adb_server,
            msg="Isolated ADB server is required for this stress test",
        )

        self._recovery_timeout = float(
            self.user_params.get(
                "recovery_timeout_sec", _DEFAULT_RECOVERY_TIMEOUT_SEC
            )
        )
        self._max_acceptable_recovery = float(
            self.user_params.get(
                "max_acceptable_recovery_sec",
                _DEFAULT_MAX_ACCEPTABLE_RECOVERY_SEC,
            )
        )
        self._recovery_times: list[float] = []

    async def teardown_class(self) -> None:
        """Logs the recovery time distribution summary."""
        if self._recovery_times:
            p50 = _calculate_percentile(self._recovery_times, 50.0)
            p95 = _calculate_percentile(self._recovery_times, 95.0)
            max_t = max(self._recovery_times)
            min_t = min(self._recovery_times)
            avg_t = sum(self._recovery_times) / len(self._recovery_times)

            _LOGGER.info(
                "=== ADB Server Restart Recovery Timing Distribution ==="
            )
            _LOGGER.info(
                "Total successful iterations: %d", len(self._recovery_times)
            )
            _LOGGER.info("Min recovery time: %.3fs", min_t)
            _LOGGER.info("Avg recovery time: %.3fs", avg_t)
            _LOGGER.info("p50 recovery time: %.3fs", p50)
            _LOGGER.info("p95 recovery time: %.3fs", p95)
            _LOGGER.info("Max recovery time: %.3fs", max_t)

        mock.patch.stopall()
        await super().teardown_class()

    async def _test_logic(self, iteration: int) -> None:
        """Executes one iteration of isolated ADB server restart."""
        _LOGGER.info("Starting ADB Server Restart iteration# %d", iteration)

        adb_server = self.dut.adb._adb_server
        assert adb_server is not None, "Isolated ADB server is required"
        asserts.assert_is_not_none(adb_server)

        # Alternate root and shell states across iterations to verify that
        # root state is preserved across host server restarts.
        expected_root = iteration % 2 == 1
        if expected_root:
            _LOGGER.info("Setting up root state for iteration %d", iteration)
            self.dut.adb.root(timeout=_DEFAULT_ROOT_TIMEOUT_SEC)
            pre_id = self.dut.adb.run(
                ["shell", "id"], timeout=_DEFAULT_CMD_TIMEOUT_SEC
            )
            asserts.assert_in("uid=0(root)", pre_id)
        else:
            _LOGGER.info("Setting up shell state for iteration %d", iteration)
            self.dut.adb.unroot(timeout=_DEFAULT_ROOT_TIMEOUT_SEC)
            pre_id = self.dut.adb.run(
                ["shell", "id"], timeout=_DEFAULT_CMD_TIMEOUT_SEC
            )
            asserts.assert_in("uid=2000(shell)", pre_id)

        # Record pre-restart process and port state.
        old_process = adb_server._process
        old_pid = old_process.pid if old_process else None
        old_port = adb_server.port()

        # Execute isolated ADB server restart and measure recovery time.
        start_time = time.monotonic()
        _LOGGER.info("Restarting isolated ADB server...")
        adb_server.restart()

        # Verify restart count accounting: restart() increments _attempt_count.
        with adb_server._lock:
            asserts.assert_equal(
                adb_server._attempt_count,
                1,
                msg=(
                    "AdbServer._attempt_count expected to be 1 after "
                    f"restart(), got {adb_server._attempt_count}"
                ),
            )

        # Exercise reset_restart_count() and verify _attempt_count resets to 0.
        adb_server.reset_restart_count()
        with adb_server._lock:
            asserts.assert_equal(
                adb_server._attempt_count,
                0,
                msg="reset_restart_count() failed to reset _attempt_count to 0",
            )

        # Bounded wait for device to re-attach with attempts=1 so the full
        # recovery timeout is given without triggering _recover_adb_server().
        _LOGGER.info(
            "Waiting for device to re-attach (timeout=%.1fs)...",
            self._recovery_timeout,
        )
        self.dut.adb.run(
            ["wait-for-device"],
            attempts=1,
            timeout=self._recovery_timeout,
        )
        recovery_time = time.monotonic() - start_time
        self._recovery_times.append(recovery_time)
        _LOGGER.info(
            "Iteration %d recovered in %.3fs (max allowed: %.1fs)",
            iteration,
            recovery_time,
            self._max_acceptable_recovery,
        )

        # Assert recovery completed within acceptable SLA budget.
        asserts.assert_less_equal(
            recovery_time,
            self._max_acceptable_recovery,
            msg=(
                f"Recovery time {recovery_time:.3f}s exceeded recovery "
                f"budget {self._max_acceptable_recovery:.1f}s"
            ),
        )

        # Verify process and port lifecycle: no orphaned process.
        new_process = adb_server._process
        assert new_process is not None, "New ADB server process is None"
        asserts.assert_is_not_none(
            new_process, msg="New ADB server process is None"
        )
        asserts.assert_is_none(
            new_process.poll(), msg="New ADB server process exited unexpectedly"
        )
        if old_pid is not None:
            asserts.assert_not_equal(
                old_pid,
                new_process.pid,
                msg="ADB server PID was not refreshed after restart",
            )

        new_port = adb_server.port()
        _LOGGER.info(
            "Isolated ADB server active on port %d (previous port: %d)",
            new_port,
            old_port,
        )

        # Verify device state: must be 'device', not offline or unauthorized.
        state_output = self.dut.adb.run(
            ["get-state"], timeout=_DEFAULT_CMD_TIMEOUT_SEC
        ).strip()
        asserts.assert_equal(
            state_output,
            "device",
            msg=f"Device in unexpected state after restart: {state_output}",
        )

        devices_output = self.dut.adb.run(
            ["devices", "-l"],
            timeout=_DEFAULT_CMD_TIMEOUT_SEC,
            include_serial=False,
        )
        asserts.assert_not_in(
            "offline",
            devices_output,
            msg=f"Device listed as offline in devices -l: {devices_output}",
        )
        asserts.assert_not_in(
            "unauthorized",
            devices_output,
            msg=(
                f"Device listed as unauthorized in devices -l: {devices_output}"
            ),
        )

        # Verify representative command execution.
        echo_marker = f"restart_check_iter_{iteration}"
        output = self.dut.adb.run(
            ["shell", "echo", echo_marker],
            timeout=_DEFAULT_CMD_TIMEOUT_SEC,
        )
        asserts.assert_in(
            echo_marker,
            output,
            msg=f"Failed to execute representative command: {output}",
        )

        # Verify root state was preserved across server restart.
        post_id = self.dut.adb.run(
            ["shell", "id"], timeout=_DEFAULT_CMD_TIMEOUT_SEC
        )
        if expected_root:
            asserts.assert_in(
                "uid=0(root)",
                post_id,
                msg=f"Root state lost across server restart: {post_id}",
            )
        else:
            asserts.assert_in(
                "uid=2000(shell)",
                post_id,
                msg=f"Shell state altered across server restart: {post_id}",
            )

        _LOGGER.info(
            "Successfully completed ADB Server Restart iteration# %d",
            iteration,
        )

    def _name_func(self, iteration: int) -> str:
        return f"test_adb_server_restart_{iteration}"


if __name__ == "__main__":
    test_runner.main()
