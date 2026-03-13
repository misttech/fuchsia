# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Fuchsia base test class."""

import contextlib
import enum
import functools
import inspect
import logging
import pathlib
from collections.abc import Iterator
from typing import Any, Callable, Coroutine, ParamSpec, TypeVar

import fuchsia_async_extension
from honeydew.auxiliary_devices.power_switch import power_switch
from honeydew.auxiliary_devices.usb_power_hub import usb_power_hub
from honeydew.fuchsia_device import fuchsia_device
from honeydew.typing import custom_types
from mobly.base_test import BaseTestClass as MoblyBaseTestClass
from mobly.records import TestResultRecord

_LOGGER: logging.Logger = logging.getLogger(__name__)

P = ParamSpec("P")
T = TypeVar("T")

# LINT.IfChange
HEALTH_CHECK_FAILURE_MESSAGE = (
    "One or more FuchsiaDevice's health check failed in "
    "teardown_test. So failing the test case..."
)
# LINT.ThenChange(//tools/testing/tefmocheck/string_in_log_check.go)


class SnapshotOn(enum.StrEnum):
    """How often we need to collect the snapshot"""

    # Once per test case.
    TEARDOWN_TEST = "teardown_test"

    # Once per test case on failure only.
    TEARDOWN_TEST_ON_FAIL = "teardown_test_on_fail"

    # Once per test class.
    TEARDOWN_CLASS = "teardown_class"

    # Once per test class on failure only.
    TEARDOWN_CLASS_ON_FAIL = "teardown_class_on_fail"

    # Do not collect snapshot
    NEVER = "never"


class TracingOn(enum.StrEnum):
    """Tracing behavior for tests.

    This user param does not support any tests that reboot the device.
    """

    # Once per test case.
    TEARDOWN_TEST = "teardown_test"

    # Once per test case on failure only.
    TEARDOWN_TEST_ON_FAIL = "teardown_test_on_fail"

    # Once per test class.
    TEARDOWN_CLASS = "teardown_class"

    # Once per test class on failure only.
    TEARDOWN_CLASS_ON_FAIL = "teardown_class_on_fail"

    # Do not collect
    NEVER = "never"


# TODO(https://fxbug.dev/488299605): Rather try to abstract commonalities of
# AsyncFuchsiaBaseTest and FuchsiaBaseTest, chcl@ chose to duplicate the
# implementations instead. FuchsiaBaseTest will soon be deleted in favor
# of AsyncFuchsiaBaseTest.


# LINT.IfChange
class FuchsiaTestCases:
    """Base class for modular test cases."""

    def __init__(self, mobly_test: "FuchsiaBaseTest"):
        self.mobly_test = mobly_test

    def setup_test(self) -> None:
        """Called before each test case."""

    def teardown_test(self) -> None:
        """Called after each test case."""

    def inject_test_cases(self) -> None:
        for attr_name, method in inspect.getmembers(self, callable):
            if attr_name.startswith("test_"):

                @functools.wraps(method)
                def wrapper(
                    *args: Any, method: Any = method, **kwargs: Any
                ) -> None:
                    try:
                        self.setup_test()
                        method(*args, **kwargs)
                    finally:
                        self.teardown_test()

                self.mobly_test.generate_tests(
                    test_logic=wrapper,
                    name_func=lambda *a, name=attr_name: name,
                    arg_sets=[()],
                )


# LINT.ThenChange(//src/testing/end_to_end/mobly_base_tests/fuchsia_base_test/__init__.py)


class FuchsiaBaseTest(MoblyBaseTestClass):
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

    TEST_CASES: list[type[FuchsiaTestCases]] | None = None

    fuchsia_devices: list[fuchsia_device.FuchsiaDevice]

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
        import fuchsia_base_test as fuchsia_base_test_init

        with self._async_devices():
            fuchsia_async_extension.get_loop().run_until_complete(
                fuchsia_base_test_init.AsyncFuchsiaBaseTest._async_pre_run(self)
            )

    def setup_class(
        self,
    ) -> None:
        """setup_class is called once before running tests.

        It does the following things:
            * Reads user params passed to the test
            * Instantiates all fuchsia devices into self.fuchsia_devices
            * Instantiates and starts tracing if specified in the user params
        """
        import fuchsia_base_test as fuchsia_base_test_init

        fuchsia_async_extension.get_loop().run_until_complete(
            fuchsia_base_test_init.AsyncFuchsiaBaseTest._async_setup_class(self)
        )
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
        import fuchsia_base_test as fuchsia_base_test_init

        with self._async_devices():
            fuchsia_async_extension.get_loop().run_until_complete(
                fuchsia_base_test_init.AsyncFuchsiaBaseTest._async_setup_test(
                    self
                )
            )

    def teardown_test(self) -> None:
        """teardown_test is called once after running each test.

        It does the following things:
            * Takes snapshot of all the fuchsia devices and stores it under
              test case directory if `snapshot_on` test param is set to
              "teardown_test"
            * Logs a info message onto device that test case has ended.
        """
        import fuchsia_base_test as fuchsia_base_test_init

        with self._async_devices():
            fuchsia_async_extension.get_loop().run_until_complete(
                fuchsia_base_test_init.AsyncFuchsiaBaseTest._async_teardown_test(
                    self
                )
            )

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
        import fuchsia_base_test as fuchsia_base_test_init

        with self._async_devices():
            fuchsia_async_extension.get_loop().run_until_complete(
                fuchsia_base_test_init.AsyncFuchsiaBaseTest._async_teardown_class(
                    self
                )
            )

    def on_fail(self, record: TestResultRecord) -> None:
        """on_fail is called once when a test case fails.

        It does the following things:
            * Takes snapshot of all the fuchsia devices and stores it under
              test case directory if `snapshot_on` test param is set to
              "on_fail"
        """
        import fuchsia_base_test as fuchsia_base_test_init

        with self._async_devices():
            fuchsia_async_extension.get_loop().run_until_complete(
                fuchsia_base_test_init.AsyncFuchsiaBaseTest._async_on_fail(
                    self, record
                )
            )

    def _output_dir(self) -> pathlib.Path:
        import fuchsia_base_test as fuchsia_base_test_init

        return fuchsia_base_test_init.AsyncFuchsiaBaseTest._output_dir(self)

    def output_file_path(self, file_name: str) -> pathlib.Path:
        import fuchsia_base_test as fuchsia_base_test_init

        return fuchsia_base_test_init.AsyncFuchsiaBaseTest.output_file_path(
            self, file_name
        )

    def on_pass(self, record: TestResultRecord) -> None:
        pass

    def on_skip(self, record: TestResultRecord) -> None:
        pass

    def _collect_snapshot(self, directory: str) -> None:
        import fuchsia_base_test as fuchsia_base_test_init

        with self._async_devices():
            fuchsia_async_extension.get_loop().run_until_complete(
                fuchsia_base_test_init.AsyncFuchsiaBaseTest._collect_snapshot(
                    self, directory
                )
            )

    def _get_controller_configs(
        self, controller_type: str
    ) -> list[dict[str, object]]:
        import fuchsia_base_test as fuchsia_base_test_init

        return (
            fuchsia_base_test_init.AsyncFuchsiaBaseTest._get_controller_configs(
                self, controller_type
            )
        )

    def _get_device_config(
        self, controller_type: str, identifier_key: str, identifier_value: str
    ) -> dict[str, object]:
        import fuchsia_base_test as fuchsia_base_test_init

        return fuchsia_base_test_init.AsyncFuchsiaBaseTest._get_device_config(
            self, controller_type, identifier_key, identifier_value
        )

    def _get_device_config_value(
        self,
        key: str,
        identifier_key: str,
        identifier_value: str,
        controller_type: str = "FuchsiaDevice",
    ) -> Any | None:
        import fuchsia_base_test as fuchsia_base_test_init

        return fuchsia_base_test_init.AsyncFuchsiaBaseTest._get_device_config_value(
            self, key, identifier_key, identifier_value, controller_type
        )

    def _health_check_and_recover(self) -> None:
        import fuchsia_base_test as fuchsia_base_test_init

        with self._async_devices():
            fuchsia_async_extension.get_loop().run_until_complete(
                fuchsia_base_test_init.AsyncFuchsiaBaseTest._health_check_and_recover(
                    self
                )
            )

    def _recover_device(self, fx_device: fuchsia_device.FuchsiaDevice) -> None:
        import fuchsia_base_test as fuchsia_base_test_init

        fuchsia_async_extension.get_loop().run_until_complete(
            fuchsia_base_test_init.AsyncFuchsiaBaseTest._recover_device(
                self, fx_device.as_async()
            )
        )

    def _lookup_power_switch(
        self, fx_device: fuchsia_device.FuchsiaDevice
    ) -> tuple[power_switch.PowerSwitch, int | None]:
        import fuchsia_base_test as fuchsia_base_test_init

        return fuchsia_base_test_init.AsyncFuchsiaBaseTest._lookup_power_switch(
            self, fx_device.as_async()
        )

    def _lookup_usb_power_hub(
        self, fx_device: fuchsia_device.FuchsiaDevice
    ) -> tuple[usb_power_hub.UsbPowerHub, int | None]:
        import fuchsia_base_test as fuchsia_base_test_init

        return (
            fuchsia_base_test_init.AsyncFuchsiaBaseTest._lookup_usb_power_hub(
                self, fx_device.as_async()
            )
        )

    def _log_message_to_devices(
        self, message: str, level: custom_types.LEVEL
    ) -> None:
        import fuchsia_base_test as fuchsia_base_test_init

        with self._async_devices():
            fuchsia_async_extension.get_loop().run_until_complete(
                fuchsia_base_test_init.AsyncFuchsiaBaseTest._log_message_to_devices(
                    self, message, level
                )
            )

    def _process_metric_user_params(self) -> None:
        import fuchsia_base_test as fuchsia_base_test_init

        fuchsia_base_test_init.AsyncFuchsiaBaseTest._process_metric_user_params(
            self
        )

    def generate_tests(
        self,
        test_logic: Callable[P, None | Coroutine[Any, Any, None]],
        name_func: Callable[P, str],
        arg_sets: list[P.args],
        uid_func: Callable[P, str] | None = None,
    ) -> None:
        if inspect.iscoroutinefunction(test_logic):

            @functools.wraps(test_logic)
            def wrapper(*t_args: P.args, **t_kwargs: P.kwargs) -> None:
                return fuchsia_async_extension.get_loop().run_until_complete(
                    test_logic(*t_args, **t_kwargs)
                )

            return super().generate_tests(
                wrapper, name_func, arg_sets, uid_func
            )
        return super().generate_tests(test_logic, name_func, arg_sets, uid_func)

    def __init_subclass__(cls) -> None:
        """Creates an async Mobly entrypoint method for each overridden async
        method and each async test method.
        """

        super().__init_subclass__()

        # The `make_sync_wrapper` closure factory safely captures the wrapped func
        # and correctly provide types for the wrapped func.
        def make_sync_wrapper(
            func: Callable[P, Coroutine[Any, Any, T]]
        ) -> Callable[P, T]:
            @functools.wraps(func)
            def wrapper(*args: P.args, **kwargs: P.kwargs) -> T:
                return fuchsia_async_extension.get_loop().run_until_complete(
                    func(*args, **kwargs)
                )

            return wrapper

        # Copy original cls.__dict__ items so the for-loop can modify attributes of cls as needed.
        dict_items = list(cls.__dict__.items())
        for attr_name, attr_value in dict_items:
            # Handle async test methods (e.g., 'async def test_my_feature').
            #
            # Mobly expects test methods to be synchronous. To support async tests,
            # we do the following:
            #   a. Rename the original async test method (e.g., 'test_my_feature')
            #      to a private name (e.g., '__async_test_my_feature').
            #   b. Replace the original method name ('test_my_feature') with a
            #      synchronous wrapper created by make_sync_wrapper.
            if attr_name.startswith("test_") and inspect.iscoroutinefunction(
                attr_value
            ):
                async_attr_name = f"__async_{attr_name}"
                setattr(cls, async_attr_name, attr_value)
                setattr(cls, attr_name, make_sync_wrapper(attr_value))
