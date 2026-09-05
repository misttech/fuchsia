# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Shared library for USB Zero Function (usb-zero-function) test suites.

Provides Zero-Function-specific constants, device node discovery, and a reusable
`ZeroFunctionBaseTest` Mobly base class utilizing generic `usb_config` and
`sysfs_usb` modules.
"""

import json
import logging
import os
import re
import shlex
import shutil
import subprocess
import sys
import time
from typing import Any, List, Optional, Tuple

import fuchsia_base_test
from mobly import asserts
from usb_lib.sysfs_usb import find_usb_device_node, wait_for_usb_device
from usb_lib.usb_config import (
    get_dut_serial,
    get_usb_config,
    parse_usb_config_functions,
    set_usb_config,
)

try:
    from .usbtest_controller import UsbTestController
except ImportError:
    from usbtest_controller import UsbTestController

_LOGGER: logging.Logger = logging.getLogger(__name__)

USB_ZERO_VID: int = 0x18D1
USB_ZERO_PID: int = 0xA022
KNOWN_TEST_DEVICES: Tuple[Tuple[int, int], ...] = (
    (USB_ZERO_VID, USB_ZERO_PID),  # Fuchsia USB Zero Function
)

ALL_SUPPORTED_TEST_IDS: Tuple[int, ...] = (
    0,
    1,
    2,
    3,
    4,
    5,
    6,
    7,
    8,
    9,
    10,
    11,
    12,
    13,
    14,
    17,
    18,
    19,
    20,
    21,
    24,
    25,
    27,
    28,
    29,
    30,
    31,
)
"""27 supported test IDs. Isochronous tests (15, 16, 22, 23, 26) are excluded because
zero function only exposes bulk/control endpoints. Loopback tests (32-35) are tested separately."""


def find_zero_function_device_node(
    dut: Optional[Any] = None,
    target_serial: Optional[str] = None,
) -> Optional[str]:
    """Scan Linux sysfs to locate the active Zero Function /dev/bus/usb/BBB/DDD node.

    Args:
        dut: Optional Honeydew FuchsiaDevice object (serial is extracted automatically if provided).
        target_serial: Optional serial number string override.

    Returns:
        Device path if found, or None.
    """
    return find_usb_device_node(
        dut=dut,
        target_serial=target_serial,
        known_devices=KNOWN_TEST_DEVICES,
    )


def wait_for_zero_function(
    dut: Optional[Any] = None,
    target_serial: Optional[str] = None,
    timeout_sec: float = 30.0,
    poll_interval_sec: float = 0.5,
) -> str:
    """Poll until the Zero Function device node is enumerated and accessible.

    Args:
        dut: Optional Honeydew FuchsiaDevice object (serial is extracted automatically if provided).
        target_serial: Optional serial number string override.
        timeout_sec: Maximum wait time in seconds.
        poll_interval_sec: Polling interval in seconds.

    Returns:
        The /dev/bus/usb/BBB/DDD device node path.
    """
    return wait_for_usb_device(
        dut=dut,
        target_serial=target_serial,
        known_devices=KNOWN_TEST_DEVICES,
        timeout_sec=timeout_sec,
        poll_interval_sec=poll_interval_sec,
    )


class ZeroFunctionBaseTest(fuchsia_base_test.FuchsiaBaseTest):
    """Base Mobly test class for USB Zero Function test suites."""

    async def setup_class(self) -> None:
        """Record the initial USB peripheral configuration and initialize test harness."""
        await super().setup_class()

        self.snapshot_on = fuchsia_base_test.SnapshotOn.NEVER
        self.tracing_on = fuchsia_base_test.TracingOn.NEVER

        # Warm up cached persistent properties needed by Mobly test metadata
        for prop in ("board", "product", "device_name"):
            try:
                _ = getattr(self.dut, prop, None)
            except Exception:
                pass

        self.target_serial = get_dut_serial(self.dut)

        self.driver_controller = UsbTestController(
            vendor=USB_ZERO_VID, product=USB_ZERO_PID, alt=0
        )
        try:
            self.driver_controller.load_driver()
        except Exception as e:
            _LOGGER.warning(f"Could not load usbtest driver: {e}")

        self._initial_config_raw: str = get_usb_config(self.dut)
        self._initial_functions: List[str] = parse_usb_config_functions(
            self._initial_config_raw
        )
        _LOGGER.info(
            f"Saved initial USB peripheral configuration: {self._initial_functions}"
        )
        self.dev_node: str = self.configure_zero_function("sourcesink;loopback")
        _LOGGER.info(f"Configured Zero Function device node: {self.dev_node}")

    async def setup_test(self) -> None:
        """Override to avoid FuchsiaBaseTest trying to log via FFX when device is in Zero Function mode."""
        self._devices_not_healthy = False
        self.test_case_path = os.path.join(
            self.log_path, self.current_test_info.name
        )
        os.makedirs(self.test_case_path, exist_ok=True)
        _LOGGER.info(f"Starting test case '{self.current_test_info.name}'")

    async def teardown_test(self) -> None:
        """Override to avoid FuchsiaBaseTest trying to run health checks via FFX when in Zero Function mode."""
        _LOGGER.info(f"Finished test case '{self.current_test_info.name}'")
        if hasattr(self, "test_case_path") and os.path.exists(
            self.test_case_path
        ):
            try:
                if len(os.listdir(self.test_case_path)) == 0:
                    os.rmdir(self.test_case_path)
            except OSError:
                pass

    async def on_fail(self, record: Any) -> None:
        """Override to record failure while preserving FatalDeviceError propagation."""
        self._any_test_failed = True
        _LOGGER.warning(f"Test case failed: {record}")
        await super().on_fail(record)

    async def _collect_snapshot(self, directory: str) -> None:
        """Bypass network snapshot collection when target is in USB peripheral test mode."""
        _LOGGER.info("Bypassing snapshot collection in Zero Function mode.")

    async def teardown_class(self) -> None:
        """Restore the initial USB peripheral configuration and clean up."""
        try:
            initial_funcs = getattr(self, "_initial_functions", None)
            if initial_funcs:
                restore_str = ",".join(initial_funcs)
                _LOGGER.info(
                    f"Restoring initial USB peripheral configuration: {restore_str}"
                )
                try:
                    set_usb_config(
                        self.dut, restore_str, reboot_if_needed=False
                    )
                except Exception as e:
                    _LOGGER.warning(
                        f"Failed to restore initial USB peripheral configuration: {e}"
                    )
            driver_ctrl = getattr(self, "driver_controller", None)
            if driver_ctrl:
                try:
                    driver_ctrl.unload_driver()
                except Exception as e:
                    _LOGGER.warning(f"Failed unloading usbtest driver: {e}")
            _LOGGER.info("Teardown class completed.")
        finally:
            await super().teardown_class()

    def configure_zero_function(self, config_name: str = "sourcesink") -> str:
        """Switch the device to Zero Function mode and wait for host enumeration."""
        old_node = getattr(self, "dev_node", None)
        set_usb_config(self.dut, config_name)
        # If an old device node was already present, wait for it to disconnect before polling.
        if old_node and os.path.exists(old_node):
            disconnect_deadline = time.monotonic() + 5.0
            while time.monotonic() < disconnect_deadline:
                if not os.path.exists(old_node):
                    break
                time.sleep(0.1)
        dev_node = wait_for_zero_function(
            dut=self.dut, target_serial=getattr(self, "target_serial", None)
        )
        self.dev_node = dev_node
        return dev_node

    def execute_testusb_cli(
        self,
        dev_node: str,
        test_ids: Optional[List[int]] = None,
        mode: str = "both",
        iterations: int = 10,
        extra_args: Optional[List[str]] = None,
        timeout_sec: float = 60.0,
        testusb_bin: Optional[str] = None,
    ) -> subprocess.CompletedProcess[str]:
        """Run the testusb runner against the specified device node via subprocess."""
        testusb_tool = (
            testusb_bin
            or os.environ.get("TESTUSB_SCRIPT")
            or shutil.which("testusb")
            or os.path.join(
                os.path.dirname(__file__), "..", "testusb", "testusb.py"
            )
        )
        cmd = (
            [sys.executable, "-u", testusb_tool]
            if testusb_tool.endswith(".py")
            else [testusb_tool]
        )
        cmd.extend(
            [
                "-D",
                dev_node,
                "-m",
                mode,
                "-c",
                str(iterations),
            ]
        )
        if test_ids:
            cmd.extend(["-t", ",".join(str(tid) for tid in test_ids)])
        if extra_args:
            cmd.extend(extra_args)

        _LOGGER.info(f"Executing testusb command: {shlex.join(cmd)}")
        try:
            res = subprocess.run(
                cmd, capture_output=True, text=True, timeout=timeout_sec
            )
        except subprocess.TimeoutExpired as e:
            _LOGGER.error(f"testusb timed out after {timeout_sec}s: {e}")
            out_str = (
                e.stdout.decode(errors="replace")
                if isinstance(e.stdout, bytes)
                else (e.stdout or "")
            )
            err_str = (
                e.stderr.decode(errors="replace")
                if isinstance(e.stderr, bytes)
                else (e.stderr or "")
            )
            if out_str:
                _LOGGER.error(f"testusb stdout before timeout:\n{out_str}")
            if err_str:
                _LOGGER.error(f"testusb stderr before timeout:\n{err_str}")
            return subprocess.CompletedProcess(
                cmd,
                returncode=124,
                stdout=out_str,
                stderr=f"TimeoutExpired after {timeout_sec}s: {err_str}",
            )
        _LOGGER.info(f"testusb exit code: {res.returncode}")
        if res.returncode != 0:
            _LOGGER.error(f"testusb stdout:\n{res.stdout}")
            if res.stderr:
                _LOGGER.error(f"testusb stderr:\n{res.stderr}")
        else:
            _LOGGER.info(f"testusb stdout:\n{res.stdout}")
            if res.stderr:
                _LOGGER.warning(f"testusb stderr:\n{res.stderr}")
        return res

    def execute_testusb_timed(
        self,
        dev_node: str,
        test_ids: Optional[List[int]] = None,
        mode: str = "both",
        duration_sec: float = 120.0,
        iterations_per_batch: int = 10,
        extra_args: Optional[List[str]] = None,
    ) -> None:
        """Run testusb repeatedly in batches until duration_sec has elapsed.

        Args:
            dev_node: Character device node (/dev/bus/usb/BBB/DDD).
            test_ids: Optional list of test numbers.
            mode: Mode ('sourcesink', 'loopback', 'both', or 'auto').
            duration_sec: Duration to sustain the stress loop in seconds.
            iterations_per_batch: Number of iterations per testusb invocation.
            extra_args: Additional command line flags for testusb.py.
        """
        start_time = time.monotonic()
        batch = 0
        total_iterations = 0

        _LOGGER.info(
            f"Starting timed stress execution: test_ids={test_ids}, mode='{mode}', "
            f"target_duration={duration_sec:.1f}s, iterations_per_batch={iterations_per_batch}"
        )

        while True:
            elapsed = time.monotonic() - start_time
            if elapsed >= duration_sec:
                break

            batch += 1
            remaining = duration_sec - elapsed
            _LOGGER.info(
                f"[Batch {batch}] Running testusb (Elapsed: {elapsed:.1f}s / {duration_sec:.1f}s, "
                f"Remaining: {remaining:.1f}s)..."
            )

            proc = self.execute_testusb_cli(
                dev_node=dev_node,
                test_ids=test_ids,
                mode=mode,
                iterations=iterations_per_batch,
                extra_args=extra_args,
            )
            self.assert_testusb_cli_success(
                proc,
                f"Stress test batch {batch} failed for test_ids={test_ids} at elapsed {elapsed:.1f}s",
            )
            total_iterations += iterations_per_batch

        total_elapsed = time.monotonic() - start_time
        _LOGGER.info(
            f"Completed timed stress test successfully: {batch} batches, "
            f"{total_iterations} total iterations across {total_elapsed:.2f}s."
        )

    def assert_testusb_cli_success(
        self, proc: subprocess.CompletedProcess[str], msg: Optional[str] = None
    ) -> None:
        """Assert that testusb CLI execution completed with returncode 0."""
        if proc.returncode != 0 and proc.stdout:
            # When all requested tests are skipped (e.g. unsupported by the host kernel
            # usbtest driver), testusb exits with non-zero (1). If no tests failed or
            # encountered errors, mark the Mobly test case as skipped instead of failing.
            m = re.search(
                r"Summary:\s*\d+\s*executed,\s*0\s*passed,\s*0\s*failed,\s*(\d+)\s*skipped,\s*0\s*errors",
                proc.stdout,
            )
            if m and int(m.group(1)) > 0:
                asserts.skip(
                    f"Test case skipped: not supported by host kernel usbtest driver:\n{proc.stdout.strip()}"
                )
            if proc.stdout.strip().startswith("{"):
                try:
                    data = json.loads(proc.stdout)
                    summary = data.get("summary", {})
                    if (
                        summary.get("passed") == 0
                        and summary.get("failed") == 0
                        and summary.get("errors") == 0
                        and summary.get("skipped", 0) > 0
                    ):
                        asserts.skip(
                            f"Test case skipped: not supported by host kernel usbtest driver:\n{proc.stdout.strip()}"
                        )
                except Exception:
                    pass

        err_detail = (
            f"\nStdout:\n{proc.stdout}\nStderr:\n{proc.stderr}"
            if proc.returncode != 0
            else ""
        )
        asserts.assert_equal(
            proc.returncode,
            0,
            f"{msg or 'testusb CLI execution failed with non-zero exit code'}: {proc.returncode}{err_detail}",
        )
