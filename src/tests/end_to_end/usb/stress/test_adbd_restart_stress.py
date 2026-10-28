# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""adbd Restart (Starnix SIGKILL) Stress Test."""

import logging
import math
import time

import fuchsia_base_test
from honeydew.device_classes import android_device
from mobly import asserts, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)

# Default timeouts in seconds.
_DEFAULT_RESPAWN_TIMEOUT_SEC: float = 60.0
_DEFAULT_CMD_TIMEOUT_SEC: float = 15.0
_DEFAULT_ROOT_TIMEOUT_SEC: float = 30.0
_DEFAULT_FFX_TIMEOUT_SEC: float = 30.0


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


class AdbdRestartStressTest(fuchsia_base_test.FuchsiaBaseTest):
    """Stress test for killing and respawning device-side adbd via SIGKILL.

    Repeatedly kills the device-side adbd process out-of-band using
    'ffx starnix kill -p <pid> -s 9', verifies that Starnix respawns it,
    validates PID updates, tests root/shell privilege persistence across
    SIGKILL, ensures the host-side ADB server is not restarted, confirms
    other transports remain healthy, and logs time-to-adbd-ready percentiles.

    Required Mobly Test Params:
        num_iterations (int): Number of kill/respawn cycles to execute.

    Optional Mobly Test Params:
        respawn_timeout_sec (float): Maximum seconds to wait for adbd to
            respawn. Defaults to 60.0 seconds.
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
        _ = self.dut.adb

        # TODO(b/564946370): Remove this unbind once AndroidDevice.adb's
        # root/unroot/use_adb_root match Honeydew ADB transport.
        for attr in ("root", "unroot", "use_adb_root"):
            self.dut.adb.__dict__.pop(attr, None)

        self._respawn_timeout = float(
            self.user_params.get(
                "respawn_timeout_sec", _DEFAULT_RESPAWN_TIMEOUT_SEC
            )
        )
        self._ffx_timeout = float(
            self.user_params.get("ffx_timeout_sec", _DEFAULT_FFX_TIMEOUT_SEC)
        )
        self._ready_times: list[float] = []

        # Validate initial adbd PID.
        initial_pids = (
            self.dut.adb.run(
                ["shell", "pidof", "adbd"], timeout=_DEFAULT_CMD_TIMEOUT_SEC
            )
            .strip()
            .split()
        )
        asserts.assert_equal(
            len(initial_pids),
            1,
            msg=f"Expected 1 adbd PID initially, got {initial_pids}",
        )
        asserts.assert_true(
            initial_pids[0].isdigit(),
            msg=f"Initial adbd PID is not an integer: {initial_pids[0]}",
        )

    async def teardown_class(self) -> None:
        """Logs the adbd ready time distribution summary."""
        if self._ready_times:
            p50 = _calculate_percentile(self._ready_times, 50.0)
            p95 = _calculate_percentile(self._ready_times, 95.0)
            max_t = max(self._ready_times)
            min_t = min(self._ready_times)
            avg_t = sum(self._ready_times) / len(self._ready_times)

            _LOGGER.info("=== adbd SIGKILL Respawn Timing Distribution ===")
            _LOGGER.info(
                "Total successful iterations: %d", len(self._ready_times)
            )
            _LOGGER.info("Min respawn time: %.3fs", min_t)
            _LOGGER.info("Avg respawn time: %.3fs", avg_t)
            _LOGGER.info("p50 respawn time: %.3fs", p50)
            _LOGGER.info("p95 respawn time: %.3fs", p95)
            _LOGGER.info("Max respawn time: %.3fs", max_t)

        await super().teardown_class()

    async def _test_logic(self, iteration: int) -> None:
        """Executes one iteration of adbd SIGKILL and respawn recovery."""
        _LOGGER.info("Starting adbd SIGKILL iteration# %d", iteration)

        # Alternate root states across iterations to verify root drop on
        # Starnix respawn and subsequent restoration.
        was_rooted = iteration % 2 == 1
        if was_rooted:
            _LOGGER.info("Ensuring root privileges before kill...")
            self.dut.adb.root(timeout=_DEFAULT_ROOT_TIMEOUT_SEC)
            pre_id = self.dut.adb.run(
                ["shell", "id"], timeout=_DEFAULT_CMD_TIMEOUT_SEC
            )
            asserts.assert_in("uid=0(root)", pre_id)
        else:
            _LOGGER.info("Ensuring shell privileges before kill...")
            self.dut.adb.unroot(timeout=_DEFAULT_ROOT_TIMEOUT_SEC)
            pre_id = self.dut.adb.run(
                ["shell", "id"], timeout=_DEFAULT_CMD_TIMEOUT_SEC
            )
            asserts.assert_in("uid=2000(shell)", pre_id)

        # Cache existing target adbd PID and host server process PID.
        pre_pids = (
            self.dut.adb.run(
                ["shell", "pidof", "adbd"], timeout=_DEFAULT_CMD_TIMEOUT_SEC
            )
            .strip()
            .split()
        )
        asserts.assert_equal(
            len(pre_pids),
            1,
            msg=f"Expected exactly 1 adbd PID before kill, got {pre_pids}",
        )
        old_pid = pre_pids[0]

        host_server_process = (
            self.dut.adb._adb_server._process
            if self.dut.adb._adb_server
            else None
        )
        host_pid_before = (
            host_server_process.pid if host_server_process else None
        )

        # Execute out-of-band SIGKILL via FFX (with fallback to direct kill if
        # FFX fails).
        _LOGGER.info(
            "Killing remote adbd (pid=%s) out-of-band via FFX...", old_pid
        )
        try:
            self.dut.ffx.run(
                ["starnix", "kill", "-p", old_pid, "-s", "9"],
                timeout=self._ffx_timeout,
            )
        except Exception as ffx_err:
            _LOGGER.warning(
                "FFX starnix kill timed out or failed (%s); falling back to "
                "direct kill",
                ffx_err,
            )
            try:
                self.dut.adb.run(
                    ["shell", "kill", "-9", old_pid],
                    timeout=_DEFAULT_CMD_TIMEOUT_SEC,
                    attempts=1,
                )
            except Exception as adb_err:
                _LOGGER.info(
                    "ADB connection dropped as expected during kill: %s",
                    adb_err,
                )

        # Record start_time right after the kill command completes.
        start_time = time.monotonic()

        # Bounded wait for adbd to respawn with a new PID.
        _LOGGER.info(
            "Waiting for adbd to respawn (timeout=%.1fs)...",
            self._respawn_timeout,
        )
        deadline = time.monotonic() + self._respawn_timeout
        new_pid: str | None = None
        while time.monotonic() < deadline:
            remaining_time = deadline - time.monotonic()
            poll_budget = min(5.0, max(1.0, remaining_time))
            try:
                self.dut.adb.run(
                    ["wait-for-device"],
                    timeout=poll_budget,
                    attempts=1,
                )
                cur_pids = (
                    self.dut.adb.run(
                        ["shell", "pidof", "adbd"],
                        timeout=poll_budget,
                        attempts=1,
                    )
                    .strip()
                    .split()
                )
                if len(cur_pids) == 1 and cur_pids[0].isdigit():
                    if cur_pids[0] != old_pid:
                        new_pid = cur_pids[0]
                        break
            except Exception:
                time.sleep(1.0)

        asserts.assert_is_not_none(
            new_pid,
            msg=(
                f"adbd failed to respawn with a new PID within "
                f"{self._respawn_timeout}s"
            ),
        )

        elapsed = time.monotonic() - start_time
        self._ready_times.append(elapsed)
        _LOGGER.info(
            "adbd respawned with new PID %s in %.3fs", new_pid, elapsed
        )

        # Assert PID changed and exactly one adbd process is running.
        asserts.assert_not_equal(
            new_pid,
            old_pid,
            msg=(
                f"adbd PID did not change after SIGKILL: "
                f"{new_pid} == {old_pid}"
            ),
        )

        # Verify other transports (FFX) remain healthy.
        _LOGGER.info("Verifying FFX transport health post-recovery...")
        self.dut.ffx.check_connection(timeout=self._ffx_timeout)

        # Verify that recovery did not require host-side ADB server restart.
        host_pid_after = (
            self.dut.adb._adb_server._process.pid
            if self.dut.adb._adb_server and self.dut.adb._adb_server._process
            else None
        )
        asserts.assert_equal(
            host_pid_before,
            host_pid_after,
            msg=(
                "Host-side ADB server unexpectedly restarted during "
                "device-side adbd recovery"
            ),
        )

        # Android's init respawns adbd preserving the root/shell state
        # configured in init (service.adb.root). Verify the user matches the
        # pre-kill state.
        respawn_id = self.dut.adb.run(
            ["shell", "id"], timeout=_DEFAULT_CMD_TIMEOUT_SEC
        )
        if was_rooted:
            asserts.assert_in(
                "uid=0(root)",
                respawn_id,
                msg=f"Expected adbd to respawn as root user: {respawn_id}",
            )
        else:
            asserts.assert_in(
                "uid=2000(shell)",
                respawn_id,
                msg=f"Expected adbd to respawn as shell user: {respawn_id}",
            )

        # Update Honeydew's cached adbd PID and verify it matches the new PID.
        self.dut.adb._cache_adbd_pid()
        asserts.assert_equal(
            self.dut.adb._cached_adbd_pid,
            new_pid,
            msg="Honeydew cached adbd PID was not updated after respawn",
        )

        # Run representative command to verify transport stability.
        echo_marker = f"adbd_restart_iter_{iteration}"
        echo_out = self.dut.adb.run(
            ["shell", "echo", echo_marker],
            timeout=_DEFAULT_CMD_TIMEOUT_SEC,
        )
        asserts.assert_in(
            echo_marker,
            echo_out,
            msg=(
                f"Failed to run representative command after respawn: "
                f"{echo_out}"
            ),
        )

        _LOGGER.info(
            "Successfully completed adbd SIGKILL iteration# %d", iteration
        )

    def _name_func(self, iteration: int) -> str:
        return f"test_adbd_restart_{iteration}"


if __name__ == "__main__":
    test_runner.main()
