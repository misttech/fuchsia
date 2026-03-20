# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Fuchsia base test class."""

import contextlib
import logging
from typing import Any, Iterator, ParamSpec, TypeVar

import fuchsia_async_extension
import fuchsia_base_test as fuchsia_base_test_init
from honeydew.auxiliary_devices.power_switch import power_switch
from honeydew.auxiliary_devices.usb_power_hub import usb_power_hub
from honeydew.fuchsia_device import fuchsia_device
from honeydew.typing import custom_types
from mobly.records import TestResultRecord

_LOGGER: logging.Logger = logging.getLogger(__name__)

P = ParamSpec("P")
T = TypeVar("T")


# Export backward compatibility aliases for tests
HEALTH_CHECK_FAILURE_MESSAGE = (
    fuchsia_base_test_init.HEALTH_CHECK_FAILURE_MESSAGE
)
SnapshotOn = fuchsia_base_test_init.SnapshotOn
TracingOn = fuchsia_base_test_init.TracingOn
FuchsiaTestCases = fuchsia_base_test_init.FuchsiaTestCases


class FuchsiaBaseTest(fuchsia_base_test_init.AsyncFuchsiaBaseTest):
    """Fuchsia-specific base test class

    Attributes:
        fuchsia_devices: List of FuchsiaDevice objects.
        test_case_path: Directory pointing to a specific test case artifacts.
        snapshot_on: `snapshot_on` test param value converted into SnapshotOn
            Enum.
        tracing_on: `tracing_on` test param value converted into TracingOn

    Required Mobly Test Params:
        snapshot_on (str): One of "teardown_class", "teardown_class_on_fail",
            "teardown_test", "on_fail".
            Default value is "teardown_class_on_fail".
        tracing_on (str): One of "teardown_class", "teardown_class_on_fail",
            "teardown_test", "on_fail", "never".
        # TODO(b/378563090): Switch the default to `teardown_class_on_fail` after
        # refactoring lacewing tests
            Default value is "never".

    Extensions for async methods:
        Test method defined as async, e.g., `async def test_*`, will be
        transformed into synchronous methods that run the original test method
        on a global event loop.
    """

    TEST_CASES: list[type[fuchsia_base_test_init.FuchsiaTestCases]] | None = None  # type: ignore[assignment]

    fuchsia_devices: list[fuchsia_device.FuchsiaDevice]  # type: ignore[assignment]

    @contextlib.contextmanager
    def _async_devices(self) -> Iterator[None]:
        if not hasattr(self, "fuchsia_devices"):
            yield
            return
        original_devices = self.fuchsia_devices
        async_devices: list[Any] = [
            device.as_async() if hasattr(device, "as_async") else device
            for device in original_devices
        ]
        self.fuchsia_devices = async_devices
        try:
            yield
        finally:
            self.fuchsia_devices = original_devices

    def pre_run(self) -> None:
        super().pre_run()

    def setup_generated_tests(self) -> None:
        super().setup_generated_tests()

    def setup_class(
        self,
    ) -> None:
        """setup_class is called once before running tests.

        It does the following things:
            * Reads user params passed to the test
            * Instantiates all fuchsia devices into self.fuchsia_devices
            * Instantiates and starts tracing if specified in the user params
        """
        super().setup_class()
        async_devices_result: list[Any] = self.fuchsia_devices
        self.fuchsia_devices = [
            device.as_sync() for device in async_devices_result
        ]

    def setup_test(self) -> None:
        """setup_test is called once before running each test.

        It does the following things:
            * Stores the current test case path into self.test_case_path
            * Logs a info message onto device that test case has started.
            * Instantiates and starts tracing if specified in the user params
        """
        with self._async_devices():
            super().setup_test()

    def teardown_test(self) -> None:
        """teardown_test is called once after running each test.

        It does the following things:
            * Takes snapshot of all the fuchsia devices and stores it under
              test case directory if `snapshot_on` test param is set to
              "teardown_test"
            * Logs a info message onto device that test case has ended.
        """
        with self._async_devices():
            super().teardown_test()

    def teardown_class(self) -> None:
        """teardown_class is called once after running all tests.

        It does the following things:
            * Takes snapshot of all the fuchsia devices and stores it under
              "<log_path>/teardown_class<_on_fail>" directory if `snapshot_on`
              test param is set to "teardown_class" or "teardown_class_on_fail".
            * Stops, terminates and downloads the trace data for all devices and stores
              it under "<log_path>/teardown_class<_on_fail>" directory if `tracing_on`
              test param is set to "teardown_class" or "teardown_class_on_fail".
        """
        with self._async_devices():
            super().teardown_class()

    def on_fail(self, record: TestResultRecord) -> None:
        """on_fail is called once when a test case fails.

        It does the following things:
            * Takes snapshot of all the fuchsia devices and stores it under
              test case directory if `snapshot_on` test param is set to
              "on_fail"
        """
        with self._async_devices():
            super().on_fail(record)

    def on_pass(self, record: TestResultRecord) -> None:
        with self._async_devices():
            super().on_pass(record)

    def on_skip(self, record: TestResultRecord) -> None:
        with self._async_devices():
            super().on_skip(record)

    def _collect_snapshot(self, directory: str) -> None:
        """Collects snapshots for all the FuchsiaDevice objects and stores them
        in the directory specified.

        Args:
            directory: Absolute path on the host where snapshot file need to be
                saved.
        """
        with self._async_devices():
            fuchsia_async_extension.get_loop().run_until_complete(
                super()._collect_snapshot(directory)
            )

    def _health_check_and_recover(self) -> None:
        """Ensure all FuchsiaDevice objects are healthy and if unhealthy perform
        a power_cycle in an attempt to recover.
        """
        with self._async_devices():
            fuchsia_async_extension.get_loop().run_until_complete(
                super()._health_check_and_recover()
            )

    def _recover_device(self, fx_device: fuchsia_device.FuchsiaDevice) -> None:
        """Try to recover the fuchsia device by power cycling it if the test has
        access to a power switch.

        Args:
            fx_device: FuchsiaDevice object
        """
        with self._async_devices():
            fuchsia_async_extension.get_loop().run_until_complete(
                super()._recover_device(fx_device.as_async())
            )

    def _lookup_power_switch(
        self, fx_device: fuchsia_device.FuchsiaDevice  # type: ignore[override]
    ) -> tuple[power_switch.PowerSwitch, int | None]:
        return super()._lookup_power_switch(fx_device.as_async())

    def _lookup_usb_power_hub(
        self, fx_device: fuchsia_device.FuchsiaDevice  # type: ignore[override]
    ) -> tuple[usb_power_hub.UsbPowerHub, int | None]:
        return super()._lookup_usb_power_hub(fx_device.as_async())

    def _log_message_to_devices(
        self, message: str, level: custom_types.LEVEL
    ) -> None:
        """Log message in all the Fuchsia devices.

        Args:
            message: Message that need to logged.
            level: Log message level.
        """
        with self._async_devices():
            fuchsia_async_extension.get_loop().run_until_complete(
                super()._log_message_to_devices(message, level)
            )
