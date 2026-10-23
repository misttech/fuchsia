# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests the firmware's fastboot capabilities."""

import logging
import re
from typing import Tuple

import fuchsia_base_test
from honeydew.transports.ffx import types as ffx_types
from mobly import asserts, signals, test_runner

# Required fastboot variables.
# - key: variable name
# - value: a list of acceptable strings for the value, or None for any value
_REQUIRED_VARS = {
    # Required for fastboot protocol communication.
    "max-download-size": None,
    "serialno": None,
    # Required for `ffx target flash` to identify boards.
    "hw-revision": None,
    # Slot information for debugging and failure analysis.
    "current-slot": ["a", "b"],
    "slot-count": ["2"],
    "slot-retry-count:a": None,
    "slot-retry-count:b": None,
    "slot-successful:a": None,
    "slot-successful:b": None,
    "slot-unbootable:a": None,
    "slot-unbootable:b": None,
    # TODO(b/308030836):: add is-userspace back after all devices support it.
    # "is-userspace": ["no"],
    # TODO(b/308030836):: add the vx- variables after all devices support it.
    # "vx-locked": ["no", "yes"],
    # "vx-unlockable": ["ephemeral", "no", "yes"],
}

# Regexp to extract variable name and value from fastboot output.
#
# Example getvar output:
# - "foo:bar"
# - "foo: bar"
# - "foo:a: bar"
# - "foo: bar baz"
# - "(bootloader) foo: bar"
#
# TODO(b/308030836): require a space between the name/arg and value, otherwise
# parsing can be ambiguous if the value itself could contain a colon. Currently
# we assume there is at most one arg per variable, and any other colon belongs
# to the value, e.g. `foo:a:b:c` results in name="foo:a", value="b:c".
_GETVAR_REGEX = re.compile(
    r"(\(bootloader\) )?(?P<name>[a-z\-]+(:[a-z0-9_.\-]+)?): ?(?P<val>.*)"
)


def parse_getvar(line: str) -> Tuple[str, str]:  # type: ignore[return]
    """Parses a `getvar` or `getvar all` output line.

    Args:
        line: a single line of `getvar` output.

    Returns:
        A tuple containing (name, value).

    Raises:
        Mobly assert if the line doesn't look like `getvar` output.
    """
    match = _GETVAR_REGEX.match(line)
    if match:
        return (match.group("name"), match.group("val"))
    asserts.fail("Failed to parse getvar output", extras={"line": line})


def ignore_line(line: str) -> bool:
    """Returns True if this fastboot output line should be ignored.

    Infra devices seem to occasionally drop off fastboot for short periods of
    time, and if we issue a command during this time we get a
    "<waiting for _serial_>" line that we should ignore (b/419262916).

    TODO(b/466453484): consider addressing this centrally in Lacewing instead.
    """
    return "waiting for" in line


class FastbootTest(fuchsia_base_test.FuchsiaBaseTest):
    async def setup_class(self) -> None:
        """Initializes all DUT(s)"""
        await super().setup_class()
        self._product_bundle_path = self.user_params["product_bundle_path"]

        # TODO(http://b/276740268#comment33): add support for rebooting into
        # fastboot here and leaving the device in fastboot mode for the entire
        # test class?
        #
        # For the time being we'll group tests that would ideally be separate
        # into single methods to minimize the number of times we have to reboot.

    async def setup_test(self) -> None:
        """Puts the device into fastboot mode before each test."""
        await super().setup_test()
        self.dut.fastboot.boot_to_fastboot_mode()

    async def teardown_test(self) -> None:
        """Puts the device back into Fuchsia mode after each test."""
        if self.dut.fastboot.is_in_fastboot_mode():
            await self.dut.fastboot.boot_to_fuchsia_mode()
        await super().teardown_test()

    def _getvar(self, expected_name: str) -> str:
        """Queries a single fastboot variable and returns its value."""
        lines = self.dut.fastboot.run(["getvar", expected_name])

        if len(lines) == 1:
            output = lines[0]
        elif len(lines) == 2 and ignore_line(lines[0]):
            output = lines[1]
        else:
            raise signals.TestFailure(
                "Unexpected getvar output",
                extras={"lines": lines},
            )
        name, value = parse_getvar(output)

        asserts.assert_equal(
            name, expected_name, "getvar name doesn't match expected"
        )
        return value

    def test_getvar(self) -> None:
        """Tests fastboot variables."""
        # Make sure each variable can also be individually queried and that the
        # value is what we expect.
        logging.info("Checking `getvar` variables")
        for expected_name, expected_values in _REQUIRED_VARS.items():
            value = self._getvar(expected_name)
            if expected_values:
                asserts.assert_in(
                    value,
                    expected_values,
                    "getvar value doesn't match expected",
                    extras={"name": expected_name},
                )

        # Make sure all the variables we care about also exist in `getvar all`.
        logging.info("Checking `getvar all`")
        getvar_all_vars = []
        for line in self.dut.fastboot.run(["getvar", "all"]):
            if ignore_line(line):
                continue
            name, _ = parse_getvar(line)
            if name != "all":
                getvar_all_vars.append(name)

        for name in _REQUIRED_VARS:
            asserts.assert_in(
                name, getvar_all_vars, "Missing variable in `getvar all`"
            )

    def test_reboot_bootloader(self) -> None:
        """Tests rebooting from fastboot mode back into fastboot mode."""
        logging.info("Rebooting from fastboot back into fastboot mode")
        # `ffx target fastboot reboot bootloader` waits for the target to
        # disconnect from USB, re-enumerate in fastboot mode, and respond to
        # `getvar` before returning.
        self.dut.ffx.run(
            cmd=[
                "--target",
                self.dut.fastboot.node_id,
                "target",
                "fastboot",
                "reboot",
                "bootloader",
            ],
            include_target=False,
            log_status_on_failure=False,
            machine=ffx_types.MachineFormat.RAW,
        )
        self.dut.fastboot.wait_for_fastboot_mode()

    async def _flash_and_boot_fuchsia(self) -> None:
        """Flashes the product bundle in fastboot mode and waits for Fuchsia mode."""
        self.dut.ffx.notify_intentional_disconnect()
        self.dut.ffx.run(
            cmd=[
                "--target",
                self.dut.fastboot.node_id,
                "target",
                "flash",
                "--product-bundle",
                self._product_bundle_path,
            ],
            include_target=False,
            log_status_on_failure=False,
            machine=ffx_types.MachineFormat.RAW,
        )
        self.dut.fastboot.wait_for_fuchsia_mode()
        await self.dut.wait_for_online()
        await self.dut.on_device_boot()

    async def test_flash_bootloader(self) -> None:
        """Tests flashing bootloader, rebooting, and flashing bootloader again."""
        logging.info("Flashing product bundle (pass 1)")
        await self._flash_and_boot_fuchsia()

        logging.info(
            "Rebooting into newly flashed bootloader and flashing again (pass 2)"
        )
        self.dut.fastboot.boot_to_fastboot_mode()
        await self._flash_and_boot_fuchsia()


if __name__ == "__main__":
    test_runner.main()
