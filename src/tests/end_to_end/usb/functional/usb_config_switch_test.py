# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Switches the DUT through every USB peripheral config.

Starting from the config the DUT boots with, e.g. `cdc,adb,vsock`, the test
runs `usb-cli set-config` for every non-empty subset of those functions, in
every order. After each switch it checks that the DUT reports the new config,
that the host sees the right interfaces, that CDC and ADB carry traffic, and
that `usb-cli diag` shows no health errors or stuck DWC3 endpoints. Finally it
restores the starting config.

This guards against a bug where each DWC3 endpoint kept the TRB ring that it
got when first enabled after boot. An endpoint that was Interrupt then wedged
once it carried traffic as Bulk. Which endpoints each function gets depends on
which function starts first, and that can change from boot to boot. So the test
logs the endpoint types and traffic, and fails (or, without ADB, warns) if a
run never hit that case. For that, it assumes that the DUT hasn't switched
configs since boot.

`usb-cli` runs on the serial console, the only transport that survives a
switch, so the Mobly config must provide `serial_socket`.
"""

import contextlib
import logging
import os
import re
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
    KERNEL_LOG_RE,
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

# What `usb-cli` prints last when it succeeds, and when it fails.
_USB_CLI_END_TOKENS = ("[usb-cli:DONE]", "usb-cli error:")
# `usb-cli diag` prints the health dashboard, then this, then Inspect data.
_DIAG_INSPECT_HEADER = "=== Device-Side USB Inspect Diagnostics ==="
# Dashboard status tags for a disconnected, unconfigured, or broken device.
_BAD_HEALTH_TAGS = (
    "[ERROR]",
    "[DISCONN]",
    "[ENUMERATING]",
    "[ADDRESS]",
    "[SUSPENDED]",
    "[UNKNOWN]",
)
# EP0 OUT and IN. Control endpoints stall by design to reject requests.
_DWC3_EP0 = ("endpoint-0x00", "endpoint-0x01")
# How `usb-cli diag` prints a DWC3 endpoint type, e.g. "2 (Bulk)".
_ENDPOINT_TYPE_RE = re.compile(r"\d+ \((\w+)\)")


def _get(node: Any, *names: str) -> dict[str, Any]:
    """Returns `node[names[0]][names[1]]...` if that is a dict, else {}."""
    for name in names:
        node = node.get(name) if isinstance(node, dict) else None
    return node if isinstance(node, dict) else {}


def parse_dwc3_endpoints(output: str) -> dict[str, Any]:
    """Returns the DWC3 `endpoints` Inspect node from `usb-cli diag` output.

    `usb-cli` prints an Inspect node as "name:", then its properties as
    "name: value", then its children, indenting each level by two spaces:

        === Synopsys DWC3 Controller Diagnostics ===
          dwc3:
            endpoints:
              endpoint-0x02:
                enabled: true
                type: 2 (Bulk)
                trb_fifo:
                  active_trbs:
                    0:
                      hardware_owned: true

    This returns nested dicts, as in Inspect's JSON:
    {"endpoint-0x02": {"enabled": True, "type": "2 (Bulk)", ...}}.
    """
    root: dict[str, Any] = {}
    # The nodes from the root down to the current one, with their indents.
    stack: list[tuple[int, dict[str, Any]]] = []
    for line in KERNEL_LOG_RE.sub("", output).splitlines():
        if line.lstrip().startswith("==="):
            # A section header. Only read the DWC3 section.
            stack = [(-1, root)] if "Synopsys DWC3" in line else []
            continue
        name, colon, value = line.strip().partition(":")
        if not stack or not colon:
            continue
        indent = len(line) - len(line.lstrip())
        while stack[-1][0] >= indent:
            stack.pop()
        node = stack[-1][1]
        value = value.strip()
        if value:
            node[name] = {"true": True, "false": False}.get(value, value)
        else:
            node[name] = {}
            stack.append((indent, node[name]))
    return _get(root, "dwc3", "endpoints")


def find_diag_problems(output: str) -> list[str]:
    """Returns the health errors and stuck endpoints in `usb-cli diag`."""
    dashboard = KERNEL_LOG_RE.sub("", output).split(_DIAG_INSPECT_HEADER)[0]
    # E.g. "  [ERROR]       VSOCK:          Endpoint error".
    errors = [
        line.strip()
        for line in dashboard.splitlines()
        if any(tag in line for tag in _BAD_HEALTH_TAGS)
    ]
    return errors + find_stuck_endpoints(parse_dwc3_endpoints(output))


def find_stuck_endpoints(endpoints: dict[str, Any]) -> list[str]:
    """Returns which of the DWC3 `endpoints` look stuck.

    An enabled endpoint is stuck if it is stalled, or if a TRB that the
    hardware still owns comes before one that it has completed. The hardware
    completes TRBs in order, so that older TRB will never complete.
    """
    if not endpoints:
        return ["usb-cli diag shows no DWC3 endpoints"]
    problems: list[str] = []
    for name in sorted(endpoints):
        endpoint = _get(endpoints, name)
        if name in _DWC3_EP0 or endpoint.get("enabled") is not True:
            continue
        if endpoint.get("stalled") is True:
            problems.append(f"{name} is stalled")
        trbs = _get(endpoint, "trb_fifo", "active_trbs")
        hwo: list[int] = []
        # Oldest first. Skip any TRB that the serial console garbled.
        for i in sorted((i for i in trbs if i.isdigit()), key=int):
            owned = _get(trbs, i).get("hardware_owned")
            if isinstance(owned, bool):
                hwo.append(int(owned))
        # Expect completed TRBs (0) before pending ones (1).
        if hwo != sorted(hwo):
            problems.append(f"{name} has TRB HWO bits {hwo}")
    return problems


def get_endpoint_types(endpoints: dict[str, Any]) -> dict[str, str]:
    """Returns the type of each enabled DWC3 endpoint but EP0, e.g. "Bulk"."""
    types: dict[str, str] = {}
    for name in sorted(endpoints):
        endpoint = _get(endpoints, name)
        match = _ENDPOINT_TYPE_RE.fullmatch(str(endpoint.get("type")))
        if name not in _DWC3_EP0 and endpoint.get("enabled") is True and match:
            types[name] = match.group(1)
    return types


def get_endpoint_bytes(endpoints: dict[str, Any]) -> dict[str, int]:
    """Returns how many bytes each DWC3 endpoint has carried since boot."""
    total: dict[str, int] = {}
    for name in endpoints:
        value = str(_get(endpoints, name).get("total_bytes"))
        if value.isdigit():
            total[name] = int(value)
    return total


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
        # The type of each DWC3 endpoint when the test first saw it enabled,
        # the last byte count of each, and each endpoint that was Interrupt
        # when first seen and then carried a full transfer as Bulk, e.g.
        # "endpoint-0x05 in cdc". That is the case that caused endpoints to
        # wedge.
        self._first_types: dict[str, str] = {}
        self._endpoint_bytes: dict[str, int] = {}
        self._triggers: list[str] = []
        matrix = build_config_matrix(self._initial)
        _LOGGER.info(
            "Testing %d USB configs: %s",
            len(matrix),
            " ".join(",".join(config) for config in matrix),
        )
        # Check the starting config too. That also records the endpoint types
        # at boot, which decide where the bug can happen.
        self._check(self._initial)
        for config in matrix:
            self._switch_to(config)
            self._check(config)
        if self._triggers:
            _LOGGER.info(
                "DWC3 endpoints that were Interrupt when first seen and then "
                "carried a full transfer as Bulk: %s",
                ", ".join(self._triggers),
            )
        elif not expects.recorder.has_error:
            # Only if the run passed. In a failed run, a stuck endpoint may
            # never have completed a full transfer.
            msg = (
                "No DWC3 endpoint that was Interrupt when first seen carried "
                "a full transfer as Bulk"
            )
            if {"cdc", "adb"} <= set(self._initial):
                asserts.fail(msg)
            # Without ADB, e.g. on `cdc,vsock`, only VSOCK reuses `cdc`'s
            # Interrupt endpoint when `cdc` starts first at boot.
            _LOGGER.warning(msg)

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
        # Last, so the rings have just carried traffic.
        with _expect_no_raises(f"[{label}] usb-cli diag"):
            self._check_diag(config)

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

    def _check_diag(self, config: list[str]) -> None:
        """Checks that `usb-cli diag` shows no errors or stuck endpoints."""
        output = self._usb_cli_diag(config)
        problems = find_diag_problems(output)
        if problems:
            # Inspect reads the TRBs one by one while the hardware completes
            # them, so a single read can be torn. Only trust a repeat.
            time.sleep(1)
            output = self._usb_cli_diag(config)
            problems = find_diag_problems(output)
        self._log_endpoint_types(",".join(config), output)
        if problems:
            _LOGGER.warning("usb-cli diag output: %s", output)
        asserts.assert_false(problems, f"usb-cli diag: {problems}")

    def _log_endpoint_types(self, label: str, output: str) -> None:
        """Logs the DWC3 endpoint types, and which could wedge.

        Those are the Bulk endpoints that were Interrupt when the test first
        saw them enabled. One only counts if it has carried a full transfer
        since the last check.
        """
        endpoints = parse_dwc3_endpoints(output)
        types = get_endpoint_types(endpoints)
        total_bytes = get_endpoint_bytes(endpoints)
        for name, kind in types.items():
            self._first_types.setdefault(name, kind)
        # Bulk endpoints that were Interrupt when first seen, with the bytes
        # that each carried since the last check. Serial can garble a count,
        # so never go below 0.
        exposed = {
            name: max(
                total_bytes.get(name, 0) - self._endpoint_bytes.get(name, 0), 0
            )
            for name, kind in types.items()
            if kind == "Bulk" and self._first_types[name] == "Interrupt"
        }
        self._endpoint_bytes.update(total_bytes)
        self._triggers += [
            f"{name} in {label}"
            for name, carried in exposed.items()
            if carried >= _TRANSFER_SIZE_BYTES
        ]
        _LOGGER.info(
            "[%s] DWC3 endpoint types: %s. Bulk, but Interrupt when first "
            "seen: %s",
            label,
            types,
            ", ".join(
                f"{name} carried {carried} bytes since the last check"
                for name, carried in exposed.items()
            )
            or "none",
        )

    def _usb_cli_diag(self, config: list[str]) -> str:
        """Runs `usb-cli diag` over SSH if CDC is up and works, else serial."""
        if "cdc" in config:
            try:
                # SSH is faster, and kernel logs don't get mixed in.
                return self.dut.ffx.run(
                    ["target", "ssh", "usb-cli diag"],
                    timeout=_COMMAND_TIMEOUT_SEC,
                    log_output=False,
                    machine=MachineFormat.RAW,
                    disable_controlmaster=True,
                )
            except honeydew_errors.HoneydewError as e:
                # E.g. a CDC endpoint is stuck. Serial doesn't use USB.
                _LOGGER.warning(
                    "usb-cli diag over SSH failed, so using serial: %s", e
                )
        return self.dut.serial.send_and_recv(
            cmd="usb-cli diag",
            stop_tokens=_USB_CLI_END_TOKENS,
            timeout_sec=_COMMAND_TIMEOUT_SEC,
            log_output=False,
        )


if __name__ == "__main__":
    test_runner.main()
