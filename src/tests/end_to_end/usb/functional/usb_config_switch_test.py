# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Switches the DUT through every USB peripheral config.

Starting from the config the DUT boots with, e.g. `cdc,adb,vsock`, the test
runs `usb-cli set-config` for every non-empty subset of those functions, in
every order. After each switch it checks that the DUT reports the new config,
that the host sees the right interfaces, and that CDC and ADB carry traffic.
Finally it restores the starting config.

`usb-cli` runs on the serial console, the only transport that survives a
switch, so the Mobly config must provide `serial_socket`.
"""

import contextlib
import logging
import os
import tempfile
import time
from collections.abc import Iterator
from typing import Any

import fuchsia_base_test
from honeydew import errors as honeydew_errors
from honeydew.device_classes import android_device
from honeydew.transports.ffx.types import MachineFormat
from mobly import asserts, expects, records, signals, test_runner
from usb_lib import sysfs_usb
from usb_lib.sysfs_usb import find_host_device
from usb_lib.usb_config import (
    FUNCTION_INTERFACES,
    FUNCTION_PIDS,
    GOOGLE_USB_VID,
    build_config_matrix,
    get_usb_config,
    parse_get_config,
    set_usb_config,
)

_LOGGER: logging.Logger = logging.getLogger(__name__)

# How long the host may take to see the DUT again after a switch.
_ENUMERATION_TIMEOUT_SEC = 30.0
# How long FFX and ADB may take to reconnect once the host sees the DUT.
_RECONNECT_TIMEOUT_SEC = 60
# How long to keep retrying `usb-cli get-config`. The serial console can drop
# output for a while, e.g. while Android boots and floods it with logs.
_GET_CONFIG_TIMEOUT_SEC = 60.0
# Timeout of every traffic command. A stuck endpoint fails rather than hangs.
_COMMAND_TIMEOUT_SEC = 60.0
_TRANSFER_SIZE_BYTES = 5 * 1024 * 1024

# `/tmp` is an in-memory tmpfs, so ADB transfers don't hit flash storage.
_ADB_REMOTE_PATH = "/tmp/usb_config_switch_test.bin"
_ADB_ECHO_TOKEN = "usb_config_switch_test"


@contextlib.contextmanager
def _expect_no_raises(message: str) -> Iterator[None]:
    """Records an exception as a test failure, and carries on.

    Does what `expects.expect_no_raises` does. Fuchsia's Mobly stub types
    that as a generator, so it can't be used in a `with` statement.
    """
    try:
        yield
    except Exception as e:
        record = records.ExceptionRecord(e)
        record.details = f"{message}: {str(record.details)}"
        _LOGGER.exception(record.details)
        expects.recorder.add_error(record)


class UsbConfigSwitchTest(fuchsia_base_test.FuchsiaBaseTest):
    """Switches through every USB config and checks that each one works."""

    dut: android_device.AndroidDevice

    async def setup_test(self) -> None:
        # Set these first, because Mobly runs teardown_test even if
        # setup_test fails.
        self._initial: list[str] = []
        self._current: list[str] = []
        self._switched: bool = False
        await super().setup_test()

        self._serial: str = await self.dut.serial_number()
        asserts.assert_true(self._serial, "The DUT has no serial number")
        configurations = self._get_config()["configurations"]
        asserts.assert_equal(
            len(configurations), 1, "Expected exactly one USB configuration"
        )
        self._initial = self._current = list(configurations[0])
        unknown = set(self._initial) - set(FUNCTION_INTERFACES)
        asserts.assert_false(
            unknown, f"No expectations for USB functions {sorted(unknown)}"
        )
        if len(self._initial) < 2:
            asserts.skip(f"Need two or more USB functions, got {self._initial}")
        if "adb" in self._initial:
            # Honeydew can only set up ADB while the DUT exposes it.
            _ = self.dut.adb

    def test_usb_config_switch(self) -> None:
        """Switches to every config made from the starting functions."""
        matrix = build_config_matrix(self._initial)
        _LOGGER.info(
            "Testing %d USB configs: %s",
            len(matrix),
            " ".join(",".join(config) for config in matrix),
        )
        # Check the starting config too.
        self._check(self._initial)
        for config in matrix:
            self._switch_to(config)
            self._check(config)

    async def teardown_test(self) -> None:
        try:
            if self._switched:
                try:
                    self._switch_to(self._initial)
                except signals.TestFailure as e:
                    # If a function bind race put CDC or ADB on a wedged
                    # endpoint, a second switch can bind in a working order.
                    _LOGGER.warning("Restore failed, retrying once: %s", e)
                    self._switch_to(self._initial)
        finally:
            # Its health check needs FFX and ADB, so restore first.
            await super().teardown_test()

    def _switch_to(self, config: list[str]) -> None:
        """Switches to `config`, and waits until the DUT is back.

        Raises:
            signals.TestFailure: The DUT didn't come back.
        """
        label = ",".join(config)
        _LOGGER.info(
            "Switching USB config from %s to %s", ",".join(self._current), label
        )
        start = time.monotonic()
        before = find_host_device(self._serial)
        if "cdc" in self._current:
            # The switch drops CDC, and with it FFX and SSH.
            self.dut.fuchsia_controller.before_usb_disconnect()
            self.dut.ffx.notify_intentional_disconnect()
        self._current = config
        self._switched = True
        set_usb_config(self.dut, label)

        # Wait for a new enumeration and SET_CONFIGURATION (when sysfs adds
        # interface directories). The PID alone isn't enough, because e.g.
        # `cdc,adb` and `adb,cdc` have the same PID.
        pid = FUNCTION_PIDS[frozenset(config)]
        deadline = time.monotonic() + _ENUMERATION_TIMEOUT_SEC
        while True:
            device = find_host_device(self._serial)
            if (
                device
                and device.pid == pid
                and device.interfaces
                and (before is None or device.dev_path != before.dev_path)
            ):
                break
            if time.monotonic() > deadline:
                raise signals.TestFailure(
                    f"The host didn't see {GOOGLE_USB_VID:04x}:{pid:04x} "
                    f"within {_ENUMERATION_TIMEOUT_SEC}s of switching to "
                    f"{label}. Host devices: "
                    f"{sysfs_usb.describe_usb_devices(target_vid=GOOGLE_USB_VID)}"
                )
            time.sleep(0.5)

        try:
            if "cdc" in config:
                # Unlike `dut.wait_for_online()`, this gives up eventually.
                self.dut.ffx.run(
                    [
                        "target",
                        "wait",
                        "--timeout",
                        str(_RECONNECT_TIMEOUT_SEC),
                    ],
                    timeout=_RECONNECT_TIMEOUT_SEC + 5,
                    log_status_on_failure=False,
                    disable_controlmaster=True,
                )
                self.dut.fuchsia_controller.after_usb_reconnect()
            if "adb" in config:
                self.dut.adb.check_connection(
                    timeout=_RECONNECT_TIMEOUT_SEC, attempts=1
                )
        except honeydew_errors.HoneydewError as e:
            raise signals.TestFailure(
                f"The DUT didn't reconnect after switching to {label}: {e}"
            ) from e
        _LOGGER.info("Switched to %s in %.1fs", label, time.monotonic() - start)

    def _check(self, config: list[str]) -> None:
        """Checks that `config` works. Records failures and carries on."""
        label = ",".join(config)
        with _expect_no_raises(f"[{label}] usb-cli get-config"):
            self._check_reported_config(config)
        with _expect_no_raises(f"[{label}] host interfaces"):
            self._check_host_interfaces(config)
        if "cdc" in config:
            with _expect_no_raises(f"[{label}] CDC traffic"):
                self._check_cdc_traffic()
        if "adb" in config:
            with _expect_no_raises(f"[{label}] ADB traffic"):
                self._check_adb_traffic()

    def _get_config(self) -> dict[str, Any]:
        """Runs `usb-cli get-config` and returns the JSON it prints."""
        deadline = time.monotonic() + _GET_CONFIG_TIMEOUT_SEC
        while True:
            output = get_usb_config(self.dut)
            reported = parse_get_config(output)
            if reported is not None:
                return reported
            if time.monotonic() > deadline:
                raise signals.TestFailure(
                    "Couldn't parse `usb-cli get-config` output for "
                    f"{_GET_CONFIG_TIMEOUT_SEC:.0f}s. Last output: {output!r}"
                )
            time.sleep(1)

    def _check_reported_config(self, config: list[str]) -> None:
        reported = self._get_config()
        asserts.assert_equal(
            reported.get("configurations"), [config], "configurations"
        )
        asserts.assert_equal(
            (reported.get("id_vendor"), reported.get("id_product")),
            (GOOGLE_USB_VID, FUNCTION_PIDS[frozenset(config)]),
            "VID, PID",
        )

    def _check_host_interfaces(self, config: list[str]) -> None:
        expected = sorted(i for f in config for i in FUNCTION_INTERFACES[f])
        # The host can list the device before all of its interfaces.
        deadline = time.monotonic() + 5
        while True:
            device = find_host_device(self._serial)
            actual = device.interfaces if device else None
            if actual == expected or time.monotonic() > deadline:
                break
            time.sleep(0.5)
        asserts.assert_equal(actual, expected, "class/subclass/protocol")

    def _check_cdc_traffic(self) -> None:
        """Pulls `_TRANSFER_SIZE_BYTES` from the DUT over SSH, i.e. CDC.

        DWC3 endpoints are directional (even = OUT, odd = IN) and CDC's
        Interrupt endpoint is IN, so only Bulk IN endpoints can be reassigned
        from Interrupt; the SSH request exercises Bulk OUT.
        """
        output = self.dut.ffx.run(
            ["target", "ssh", f'printf "X%{_TRANSFER_SIZE_BYTES - 2}sX" ""'],
            timeout=_COMMAND_TIMEOUT_SEC,
            log_output=False,
            machine=MachineFormat.RAW,
            disable_controlmaster=True,
        )
        asserts.assert_equal(len(output), _TRANSFER_SIZE_BYTES, "bytes")

    def _check_adb_traffic(self) -> None:
        """Round-trips `_TRANSFER_SIZE_BYTES` of random data over ADB."""
        output = self._adb("shell", "echo", _ADB_ECHO_TOKEN)
        asserts.assert_in(_ADB_ECHO_TOKEN, output, "adb shell echo")
        payload = os.urandom(_TRANSFER_SIZE_BYTES)
        with tempfile.TemporaryDirectory() as tmp:
            sent = os.path.join(tmp, "sent.bin")
            received = os.path.join(tmp, "received.bin")
            with open(sent, "wb") as f:
                f.write(payload)
            try:
                self._adb("push", "-Z", sent, _ADB_REMOTE_PATH)
                self._adb("pull", "-Z", _ADB_REMOTE_PATH, received)
            finally:
                with contextlib.suppress(honeydew_errors.HoneydewError):
                    self._adb("shell", "rm", "-f", _ADB_REMOTE_PATH)
            with open(received, "rb") as f:
                data = f.read()
            asserts.assert_equal(
                len(data), len(payload), "Pulled file size mismatch"
            )
            asserts.assert_true(
                data == payload, "The pulled file contents differ"
            )

    def _adb(self, *args: str) -> str:
        return self.dut.adb.run(
            list(args), timeout=_COMMAND_TIMEOUT_SEC, attempts=1
        )


if __name__ == "__main__":
    test_runner.main()
