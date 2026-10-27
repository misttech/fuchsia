# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Fuchsia USB peripheral configuration management library.

Provides utilities for querying, parsing, applying, and restoring USB peripheral
configurations on Fuchsia devices via Honeydew FFX transports and `usb-cli`,
as well as extracting target device identifiers directly from Honeydew device
objects.
"""

import asyncio
import collections.abc
import concurrent.futures
import inspect
import itertools
import json
import logging
import re
import shlex
from typing import Any

from honeydew import errors as honeydew_errors
from honeydew.fuchsia_device import fuchsia_device

_LOGGER: logging.Logger = logging.getLogger(__name__)
_DONE_TOKEN: str = "[usb-cli:DONE]"
_ERROR_TOKEN: str = "[usb-cli:ERROR]"
_IDENT_RE: re.Pattern[str] = re.compile(r"^[a-zA-Z0-9_-]+$")

GOOGLE_USB_VID: int = 0x18D1

# Expected USB PID for each subset of functions (e.g. `cdc,adb` and `adb,cdc`
# share 0xa026; build_config_matrix still tests every ordering).
FUNCTION_PIDS: dict[frozenset[str], int] = {
    frozenset({"cdc"}): 0xA020,
    frozenset({"adb"}): 0xA025,
    frozenset({"vsock"}): 0xA028,
    frozenset({"cdc", "adb"}): 0xA026,
    frozenset({"cdc", "vsock"}): 0xA029,
    frozenset({"adb", "vsock"}): 0xA02A,
    frozenset({"cdc", "adb", "vsock"}): 0xA02B,
}

# Class/subclass/protocol of each function's interfaces, as the host sees them.
FUNCTION_INTERFACES: dict[str, list[str]] = {
    "cdc": ["02/06/00", "0a/00/00"],  # Communication and data interfaces.
    "adb": ["ff/42/01"],
    "vsock": ["ff/43/00"],
}

# A kernel log line on the serial console, e.g.
# "[00024.593] 15009:35497> [driver, usb-phy] INFO: ...". The kernel can print
# one in the middle of a line of command output, so drop its newline too.
KERNEL_LOG_RE: re.Pattern[str] = re.compile(r"\[\d+\.\d+\] \d+:\d+>[^\n]*\n?")


def build_config_matrix(functions: list[str]) -> list[list[str]]:
    """Returns every non-empty subset of `functions`, in every order."""
    return [
        list(p)
        for k in range(1, len(functions) + 1)  # Subset size: 1, 2, ..., n.
        for p in itertools.permutations(functions, k)  # Ordered picks of k.
    ]


def parse_get_config(output: str) -> dict[str, Any] | None:
    """Returns the JSON object that `usb-cli get-config` printed, if any.

    The serial console also echoes the command and interleaves kernel log
    lines, even inside the JSON. So drop the log lines, then try each `{`
    until one starts an object with "configurations" or "functions".
    """
    output = KERNEL_LOG_RE.sub("", output)
    decoder = json.JSONDecoder()
    for start, char in enumerate(output):
        if char != "{":
            continue
        try:
            value, _ = decoder.raw_decode(output, start)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and (
            "configurations" in value or "functions" in value
        ):
            return value
    return None


def _invoke_maybe_async(
    callable_obj: collections.abc.Callable[[], Any],
    timeout_sec: float = 30.0,
) -> Any:
    """Invoke a sync callable or async coroutine function safely."""
    try:
        loop = asyncio.get_running_loop()
    except RuntimeError:
        loop = None

    def _call_worker() -> Any:
        val = callable_obj()
        if inspect.iscoroutine(val):
            return asyncio.run(val)
        return val

    if loop and loop.is_running():
        pool = concurrent.futures.ThreadPoolExecutor(max_workers=1)
        try:
            return pool.submit(_call_worker).result(timeout=timeout_sec)
        finally:
            pool.shutdown(wait=False, cancel_futures=True)
    else:
        return _call_worker()


def get_dut_serial(dut: fuchsia_device.FuchsiaDevice | None) -> str | None:
    """Resolve the hardware serial number of a Honeydew FuchsiaDevice.

    Tries, in order:
    1. `dut.serial_number()`, which resolves over FIDL and therefore needs the
       device to still be reachable.
    2. `dut.ffx.get_target_info_from_target_show()`.

    These tests deliberately tear down the USB functions that carry FIDL and
    SSH, so callers should resolve the serial once up front while the device
    is still reachable and then pass it around, rather than relying on the
    later tiers to work mid-test.

    Args:
        dut: FuchsiaDevice object from Honeydew.

    Returns:
        The device serial number string, or None if unavailable.
    """
    if dut is None:
        return None

    # 1. Resolve over FIDL. Honeydew caches the result after the first call.
    try:
        serial_number = _invoke_maybe_async(dut.serial_number, timeout_sec=5.0)
        if isinstance(serial_number, str) and serial_number:
            return serial_number.strip()
    except (honeydew_errors.HoneydewError, OSError, TimeoutError) as e:
        _LOGGER.debug("Failed calling dut.serial_number(): %s", e)

    # 2. Ask ffx what it knows about the target.
    try:
        info = dut.ffx.get_target_info_from_target_show()
        dev_sn = getattr(getattr(info, "device", None), "serial_number", None)
        if dev_sn:
            return str(dev_sn).strip()
    except (honeydew_errors.HoneydewError, OSError) as e:
        _LOGGER.debug("Failed querying ffx target info for serial: %s", e)

    return None


def get_usb_config(dut: fuchsia_device.FuchsiaDevice) -> str:
    """Retrieve the current USB peripheral policy configuration from the DUT.

    Queries the device deterministically over the serial console.

    Args:
        dut: Fuchsia DUT device object.

    Returns:
        The raw configuration string from usb-cli get-config.

    Raises:
        RuntimeError: If no serial console is available or the command could
            not be dispatched over serial.
    """
    _LOGGER.info("Querying current USB peripheral configuration on target...")

    res = _send_serial_command("usb-cli get-config", timeout_sec=5.0, dut=dut)
    if res is None:
        raise RuntimeError(
            "Failed to query USB config from the device over serial console."
        )
    if (
        _DONE_TOKEN in res
        or "{" in res
        or "functions" in res
        or "configurations" in res
    ):
        return res.strip()

    return ""


def normalize_usb_functions(functions: list[str]) -> list[str]:
    """Normalize function names, deduplicating while preserving order.

    Args:
        functions: List of USB peripheral function names (e.g. ['cdc',
            'vsock']).

    Returns:
        A deduplicated and normalized list of function names.
    """
    if not functions:
        return []
    return list(dict.fromkeys(functions))


def parse_usb_config_functions(config_str: str) -> list[str]:
    """Parse list of active function names from usb-cli get-config output.

    Args:
        config_str: Output string from usb-cli get-config.

    Returns:
        List of function names (e.g. ['cdc', 'adb', 'test']).
    """
    raw_functions: list[str] = []
    # Try structured extraction via parse_get_config first, which strips
    # interleaved serial console/kernel logs and parses JSON safely.
    data = parse_get_config(config_str)
    if data:
        if "configurations" in data and isinstance(
            data["configurations"], list
        ):
            raw_functions = [
                str(f)
                for cfg in data["configurations"]
                if isinstance(cfg, list)
                for f in cfg
            ]
        elif "functions" in data and isinstance(data["functions"], list):
            raw_functions = [str(f) for f in data["functions"]]

    if not raw_functions:
        try:
            # usb-cli get-config outputs JSON: {"functions": ["cdc", "adb", ...]}
            start_idx = config_str.find("{")
            end_idx = config_str.rfind("}")
            if start_idx != -1 and end_idx != -1 and start_idx < end_idx:
                data = json.loads(config_str[start_idx : end_idx + 1])
                if "configurations" in data and isinstance(
                    data["configurations"], list
                ):
                    raw_functions = [
                        str(f)
                        for cfg in data["configurations"]
                        if isinstance(cfg, list)
                        for f in cfg
                    ]
                elif "functions" in data and isinstance(
                    data["functions"], list
                ):
                    raw_functions = [str(f) for f in data["functions"]]
        except (json.JSONDecodeError, KeyError, TypeError) as e:
            _LOGGER.debug(
                "Failed to parse JSON configuration (%s) from: %s",
                e,
                config_str,
            )

    if not raw_functions:
        # Fallback to comma-separated splitting with identifier validation
        cleaned = (
            config_str.strip()
            .replace(_DONE_TOKEN, "")
            .replace("[", "")
            .replace("]", "")
            .replace('"', "")
        )
        raw_functions = [
            p.strip()
            for p in cleaned.split(",")
            if p.strip()
            and p.strip() != "usb-cli:DONE"
            and _IDENT_RE.match(p.strip())
        ]

    return normalize_usb_functions(raw_functions)


def _send_serial_command(
    cmd: str,
    timeout_sec: float = 5.0,
    dut: fuchsia_device.FuchsiaDevice | None = None,
) -> str | None:
    """Send a command over the Honeydew device serial transport.

    Note on framing: the infra serial server only forwards complete
    newline-terminated lines to the UART, so the command *must* end in `\\n`.
    Honeydew's `Serial.send()` already wraps commands in `\\r\\n`; a hand-rolled
    bare `\\r` is silently swallowed and never reaches the console.

    Note on reading: `Serial.read()` explicitly makes no guarantees that output
    produced by a preceding `send()` is available yet, and each call opens a
    fresh connection to the serial server, does a single `recv()` and closes.
    Output can therefore be delayed or lost between polls. Callers must treat
    an empty result as inconclusive rather than as a failure, and verify the
    effect of the command by other means.

    Args:
        cmd: Shell command string to execute over the serial console.
        timeout_sec: Maximum duration in seconds to wait for command output.
        dut: Honeydew FuchsiaDevice instance managing the target device.

    Returns:
        The console output observed after dispatching the command, which may
        be an empty string if nothing was captured in time. None if the
        command could not be dispatched at all, i.e. the device has no serial
        transport configured or the send itself failed.
    """
    if dut is None:
        return None

    try:
        serial = dut.serial
    except honeydew_errors.HoneydewError as e:
        # Raised when 'serial_socket' was not supplied during device init.
        _LOGGER.debug("No serial transport configured for the DUT: %s", e)
        return None

    if serial is None:
        return None

    try:
        output = serial.send_and_recv(
            cmd=cmd.strip(),
            stop_tokens=(_DONE_TOKEN, _ERROR_TOKEN),
            timeout_sec=timeout_sec,
        )
    except (honeydew_errors.HoneydewError, OSError) as e:
        _LOGGER.warning("Failed dispatching serial command %r: %s", cmd, e)
        return None

    _LOGGER.info(
        "Dispatched serial command %r; console output: %s",
        cmd,
        output.strip() or "<none captured>",
    )
    return output


def _notify_intentional_disconnect(dut: fuchsia_device.FuchsiaDevice) -> None:
    """Combines multiple transport disconnect notifications."""
    dut.fuchsia_controller.before_usb_disconnect()
    dut.ffx.notify_intentional_disconnect()


def set_usb_config(
    dut: fuchsia_device.FuchsiaDevice,
    config: str,
) -> None:
    """Apply a new USB peripheral configuration on the DUT via usb-cli over serial.

    The command is dispatched deterministically over the serial console because
    serial is the only transport that survives the CDC Ethernet teardown caused
    by switching USB peripheral configurations.

    Every switch tears down all USB functions and re-enumerates, so if CDC is
    currently up this drops the FFX and SSH connections. The FFX monitor is
    therefore always notified first that the upcoming disconnect is
    intentional.

    Success is deliberately *not* gated on observing usb-cli's completion
    token, because Honeydew's serial `read()` offers no synchronization with
    `send()`, so the acknowledgement is frequently missed (see
    `_send_serial_command`). Callers confirm the outcome out-of-band instead,
    by waiting for the expected USB device to enumerate on the host.

    Args:
        dut: Fuchsia DUT device object.
        config: Function or configuration string (e.g. 'cdc,adb',
            'sourcesink', 'loopback').

    Raises:
        RuntimeError: If no serial console is available or the USB
            configuration command could not be dispatched over serial.
    """
    # The switch drops CDC, and with it FFX and SSH.
    _notify_intentional_disconnect(dut)

    _LOGGER.info(
        "Applying USB peripheral configuration '%s' on target...", config
    )

    is_test = any(
        f in config.lower() for f in ("sourcesink", "loopback", "test")
    )
    usb_cli_cmd = f"usb-cli set-config {shlex.quote(config)}"

    serial_res = _send_serial_command(usb_cli_cmd, timeout_sec=8.0, dut=dut)
    if serial_res is None:
        raise RuntimeError(
            f"Failed to dispatch USB config '{config}' to the device over "
            f"serial console (serial_res={serial_res!r})"
        )

    acknowledged = _DONE_TOKEN in serial_res or "Cold reboot" in serial_res
    if is_test and not acknowledged:
        _LOGGER.info(
            "usb-cli did not acknowledge '%s' on the console; relying on "
            "host-side USB enumeration to confirm the switch.",
            config,
        )
