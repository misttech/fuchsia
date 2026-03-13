# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Async Fuchsia base test class."""

import functools
import importlib
import inspect
import logging
import os
import pathlib
import typing
from typing import Any, Callable, Coroutine, Dict, ParamSpec, TypeVar, Union

import fuchsia_async_extension
from honeydew import errors
from honeydew.auxiliary_devices.power_switch import (
    power_switch,
    power_switch_using_dmc,
)
from honeydew.auxiliary_devices.usb_power_hub import (
    usb_power_hub,
    usb_power_hub_using_dmc,
)
from honeydew.fuchsia_device import async_fuchsia_device
from honeydew.typing import custom_types
from mobly import signals, test_runner
from mobly.records import TestResultRecord
from mobly_controller import fuchsia_device as fuchsia_device_mobly_controller

# Import enums from the synchronous base test to maintain compatibility
from .fuchsia_base_test import FuchsiaTestCases, SnapshotOn, TracingOn

_LOGGER: logging.Logger = logging.getLogger(__name__)

P = ParamSpec("P")
T = TypeVar("T")

# TODO(https://fxbug.dev/488299605): Rather try to abstract commonalities of
# AsyncFuchsiaBaseTest and FuchsiaBaseTest, chcl@ chose to duplicate the
# implementations instead. FuchsiaBaseTest will soon be deleted in favor
# of AsyncFuchsiaBaseTest.


# LINT.IfChange
class AsyncFuchsiaTestCases:
    """Base class for modular test cases."""

    def __init__(
        self,
        mobly_test: "AsyncFuchsiaBaseTest",
    ):
        self.mobly_test = mobly_test

    async def setup_test(self) -> None:
        """Called before each test case."""

    async def teardown_test(self) -> None:
        """Called after each test case."""

    def inject_test_cases(self) -> None:
        for attr_name, method in inspect.getmembers(self, callable):
            if attr_name.startswith("test_"):

                @functools.wraps(method)
                async def wrapper(
                    *args: Any, method: Any = method, **kwargs: Any
                ) -> None:
                    try:
                        await self.setup_test()

                        if inspect.iscoroutinefunction(method):
                            await method(*args, **kwargs)
                        else:
                            method(*args, **kwargs)
                    finally:
                        await self.teardown_test()

                self.mobly_test.generate_tests(
                    test_logic=wrapper,
                    name_func=lambda *a, name=attr_name: name,
                    arg_sets=[()],
                )


class AsyncFuchsiaBaseTest(fuchsia_async_extension.AsyncBaseTestClass):
    """Async Fuchsia-specific base test class

    Attributes:
        fuchsia_devices: List of AsyncFuchsiaDevice objects.
        test_case_path: Directory pointing to a specific test case artifacts.
        snapshot_on: `snapshot_on` test param value converted into SnapshotOn Enum.
        tracing_on: `tracing_on` test param value converted into TracingOn Enum.

    Required Mobly Test Params:
        snapshot_on (str): One of "teardown_class", "teardown_class_on_fail",
            "teardown_test", "on_fail".
            Default value is "teardown_class_on_fail".
        tracing_on (str): One of "teardown_class", "teardown_class_on_fail",
            "teardown_test", "on_fail", "never".
            Default value is "never".
    """

    TEST_CASES: list[type[AsyncFuchsiaTestCases]] | None = None

    async def pre_run(self) -> None:
        if self.TEST_CASES is None:
            return

        for tc in self.TEST_CASES:
            tc(self).inject_test_cases()

    async def setup_class(self) -> None:
        """setup_class is called once before running tests.

        It does the following things:
            * Reads user params passed to the test
            * Instantiates all fuchsia devices into self.fuchsia_devices (as async devices)
            * Instantiates and starts tracing if specified in the user params
        """
        self._any_test_failed: bool = False
        self._process_metric_user_params()
        # We define teardown_class artifacts path here so it can be used by
        # child test classes in teardown_class before calling the super() teardown
        self._teardown_class_artifacts: str = f"{self.log_path}/teardown_class"

        fuchsia_devices_sync = await self.register_controller(
            fuchsia_device_mobly_controller
        )
        self.fuchsia_devices: list[async_fuchsia_device.AsyncFuchsiaDevice] = [
            device.as_async() for device in fuchsia_devices_sync
        ]

        if (
            self.tracing_on == TracingOn.TEARDOWN_CLASS
            or self.tracing_on == TracingOn.TEARDOWN_CLASS_ON_FAIL
        ):
            for device in self.fuchsia_devices:
                device.tracing.initialize(categories=self.trace_categories)
                await device.tracing.start()

    async def setup_test(self) -> None:
        """setup_test is called once before running each test.

        It does the following things:
            * Stores the current test case path into self.test_case_path
            * Logs a info message onto device that test case has started.
            * Instantiates and starts tracing if specified in the user params
        """
        self._devices_not_healthy: bool = False

        self.test_case_path: str = (
            f"{self.log_path}/{self.current_test_info.name}"
        )
        os.mkdir(self.test_case_path)
        await self._log_message_to_devices(
            message=f"Started executing '{self.current_test_info.name}' "
            f"Lacewing test case...",
            level=custom_types.LEVEL.INFO,
        )
        for device in self.fuchsia_devices:
            if (
                not device.tracing.is_active()
                and not device.tracing.is_session_initialized()
            ):
                if (
                    self.tracing_on == TracingOn.TEARDOWN_TEST
                    or self.tracing_on == TracingOn.TEARDOWN_TEST_ON_FAIL
                ):
                    device.tracing.initialize(categories=self.trace_categories)
                    await device.tracing.start()

    async def teardown_test(self) -> None:
        """teardown_test is called once after running each test.

        It does the following things:
            * Takes snapshot of all the fuchsia devices and stores it under
              test case directory if `snapshot_on` test param is set to
              "teardown_test"
            * Logs a info message onto device that test case has ended.
        """
        await self._health_check_and_recover()

        if self.snapshot_on == SnapshotOn.TEARDOWN_TEST:
            await self._collect_snapshot(directory=self.test_case_path)

        _LOGGER.info("Closing any active tracing sessions.")
        for device in self.fuchsia_devices:
            if (
                device.tracing.is_active()
                and device.tracing.is_session_initialized()
            ):
                if self.tracing_on == TracingOn.TEARDOWN_TEST:
                    await device.tracing.stop()
                    await device.tracing.terminate_and_download(
                        directory=self.test_case_path
                    )

        _LOGGER.info("Completed closing active tracing sessions.")
        await self._log_message_to_devices(
            message=f"Finished executing '{self.current_test_info.name}' "
            f"Lacewing test case...",
            level=custom_types.LEVEL.INFO,
        )
        if len(os.listdir(self.test_case_path)) == 0:
            os.rmdir(self.test_case_path)

        if self._devices_not_healthy:
            message: str = (
                "One or more FuchsiaDevice's health check failed in "
                "teardown_test. So failing the test case..."
            )
            _LOGGER.warning(message)
            raise signals.TestFailure(message)

    async def teardown_class(self) -> None:
        """teardown_class is called once after running all tests.

        It does the following things:
            * Takes snapshot of all the fuchsia devices and stores it under
              "<log_path>/teardown_class<_on_fail>" directory if `snapshot_on`
              test param is set to "teardown_class" or "teardown_class_on_fail".
            * Stops, terminates and downloads the trace data for all devices and stores
              it under "<log_path>/teardown_class<_on_fail>" directory if `tracing_on`
              test param is set to "teardown_class" or "teardown_class_on_fail".
        """
        for device in self.fuchsia_devices:
            if (
                device.tracing.is_active()
                and device.tracing.is_session_initialized()
            ):
                if self.tracing_on == TracingOn.TEARDOWN_CLASS:
                    await device.tracing.stop()
                    await device.tracing.terminate_and_download(
                        directory=self._teardown_class_artifacts
                    )
                elif (
                    self.tracing_on == TracingOn.TEARDOWN_CLASS_ON_FAIL
                    and self._any_test_failed
                ):
                    await device.tracing.stop()
                    await device.tracing.terminate_and_download(
                        directory=self._teardown_class_artifacts
                    )

        if self.snapshot_on == SnapshotOn.TEARDOWN_CLASS:
            self._teardown_class_artifacts = f"{self.log_path}/teardown_class"
            await self._collect_snapshot(
                directory=self._teardown_class_artifacts
            )
        elif (
            self.snapshot_on == SnapshotOn.TEARDOWN_CLASS_ON_FAIL
            and self._any_test_failed
        ):
            self._teardown_class_artifacts = (
                f"{self.log_path}/teardown_class_on_fail"
            )
            await self._collect_snapshot(
                directory=self._teardown_class_artifacts
            )

    async def on_fail(self, record: TestResultRecord) -> None:
        """on_fail is called once when a test case fails.

        It does the following things:
            * Takes snapshot of all the fuchsia devices and stores it under
              test case directory if `snapshot_on` test param is set to
              "on_fail"
        """
        self._any_test_failed = True
        if self.snapshot_on == SnapshotOn.TEARDOWN_TEST_ON_FAIL:
            await self._collect_snapshot(directory=self.test_case_path)

        for device in self.fuchsia_devices:
            if (
                device.tracing.is_active()
                and device.tracing.is_session_initialized()
            ):
                if self.tracing_on == TracingOn.TEARDOWN_TEST_ON_FAIL:
                    await device.tracing.stop()
                    await device.tracing.terminate_and_download(
                        directory=self.test_case_path
                    )

    def _output_dir(self) -> pathlib.Path:
        if hasattr(self, "test_case_path"):
            return pathlib.Path(self.test_case_path)
        elif hasattr(self, "log_path"):
            return pathlib.Path(self.log_path)
        else:
            raise RuntimeError(
                "Neither self.test_case_path nor self.log_path exist: Has setup_class or setup_test been called yet?"
            )

    def output_file_path(self, file_name: str) -> pathlib.Path:
        return self._output_dir().joinpath(file_name)

    async def on_pass(self, record: TestResultRecord) -> None:
        pass

    async def on_skip(self, record: TestResultRecord) -> None:
        pass

    async def _collect_snapshot(self, directory: str) -> None:
        """Collects snapshots for all the FuchsiaDevice objects and stores them
        in the directory specified.

        Args:
            directory: Absolute path on the host where snapshot file need to be
                saved.
        """
        if not hasattr(self, "fuchsia_devices"):
            return

        _LOGGER.info(
            "Collecting snapshots of all the AsyncFuchsiaDevice objects in '%s'...",
            self.snapshot_on.value,
        )
        for fx_device in self.fuchsia_devices:
            try:
                await fx_device.snapshot(directory=directory)
            except Exception as err:
                _LOGGER.exception(
                    "Unable to take snapshot of %s. Failed with error: %s",
                    fx_device.device_name,
                    err,
                )

    def _get_controller_configs(
        self, controller_type: str
    ) -> list[dict[str, object]]:
        for (
            controller_name,
            controller_configs,
        ) in self.controller_configs.items():
            if controller_name == controller_type:
                return controller_configs
        return []

    def _get_device_config(
        self, controller_type: str, identifier_key: str, identifier_value: str
    ) -> dict[str, object]:
        for controller_config in self._get_controller_configs(controller_type):
            if controller_config[identifier_key] == identifier_value:
                _LOGGER.info(
                    "Device configuration associated with %s is %s",
                    identifier_value,
                    controller_config,
                )
                return controller_config
        return {}

    def _get_device_config_value(
        self,
        key: str,
        identifier_key: str,
        identifier_value: str,
        controller_type: str = "FuchsiaDevice",
    ) -> Any | None:
        config: Dict[str, Any] = self._get_device_config(
            controller_type=controller_type,
            identifier_key=identifier_key,
            identifier_value=identifier_value,
        )

        return config.get(key) if config else None

    async def _health_check_and_recover(self) -> None:
        """Ensure all AsyncFuchsiaDevice objects are healthy and if unhealthy perform
        a power_cycle in an attempt to recover.
        """
        _LOGGER.info(
            "Performing health checks on all the AsyncFuchsiaDevice objects..."
        )

        for fx_device in self.fuchsia_devices:
            try:
                fx_device.health_check()
            except errors.HealthCheckError as err:
                self._devices_not_healthy = True
                _LOGGER.warning(
                    "Health check on %s failed with error '%s', will try to recover the device",
                    fx_device.device_name,
                    err,
                )
                await self._recover_device(fx_device)

        _LOGGER.info(
            "Successfully performed health checks and/or recoveries on all the "
            "AsyncFuchsiaDevice objects..."
        )

    async def _recover_device(
        self, fx_device: async_fuchsia_device.AsyncFuchsiaDevice
    ) -> None:
        """Try to recover the fuchsia device by power cycling it if the test has
        access to a power switch.

        Args:
            fx_device: AsyncFuchsiaDevice object
        """
        try:
            switch, outlet = self._lookup_power_switch(fx_device)
            await fx_device.power_cycle(power_switch=switch, outlet=outlet)
        except power_switch_using_dmc.PowerSwitchDmcError as err:
            _LOGGER.warning(
                "Unable to power cycle %s as test does not have access to DMC. "
                "Aborting the test class...",
                fx_device.device_name,
            )
            raise signals.TestAbortClass(
                f"{fx_device.device_name} is unhealthy and unable to recover it"
            ) from err
        except power_switch.PowerSwitchError as err:
            _LOGGER.warning(
                "Power cycling %s failed with error '%s'. "
                "Aborting the test class...",
                fx_device.device_name,
                err,
            )
            raise signals.TestAbortClass(
                f"{fx_device.device_name} is unhealthy and failed to recover it"
            ) from err

    def _lookup_power_switch(
        self, fx_device: async_fuchsia_device.AsyncFuchsiaDevice
    ) -> tuple[power_switch.PowerSwitch, int | None]:
        device_config: dict[str, object] = self._get_device_config(
            controller_type="FuchsiaDevice",
            identifier_key="name",
            identifier_value=fx_device.device_name,
        )
        power_switch_hw = typing.cast(
            dict[str, str], device_config.get("power_switch_hw", {})
        )
        power_switch_impl = typing.cast(
            dict[str, str], device_config.get("power_switch_impl", {})
        )
        power_switch_outlet = typing.cast(
            Union[int, None], device_config.get("power_switch_outlet", None)
        )
        if power_switch_hw and power_switch_impl:
            power_switch_class: type[power_switch.PowerSwitch] = getattr(
                importlib.import_module(power_switch_impl["module"]),
                power_switch_impl["class"],
            )
            return power_switch_class(**power_switch_hw), power_switch_outlet
        else:
            return (
                power_switch_using_dmc.PowerSwitchUsingDmc(
                    device_name=fx_device.device_name,
                ),
                None,
            )

    def _lookup_usb_power_hub(
        self, fx_device: async_fuchsia_device.AsyncFuchsiaDevice
    ) -> tuple[usb_power_hub.UsbPowerHub, int | None]:
        device_config: dict[str, object] = self._get_device_config(
            controller_type="FuchsiaDevice",
            identifier_key="name",
            identifier_value=fx_device.device_name,
        )
        usb_power_hub_hw = typing.cast(
            dict[str, str], device_config.get("usb_power_hub_hw", {})
        )
        usb_power_hub_impl = typing.cast(
            dict[str, str], device_config.get("usb_power_hub_impl", {})
        )
        usb_power_hub_port = typing.cast(
            Union[int, None], device_config.get("usb_power_hub_port", None)
        )
        if usb_power_hub_hw and usb_power_hub_impl:
            usb_power_hub_class: type[usb_power_hub.UsbPowerHub] = getattr(
                importlib.import_module(usb_power_hub_impl["module"]),
                usb_power_hub_impl["class"],
            )
            return usb_power_hub_class(**usb_power_hub_hw), usb_power_hub_port
        else:
            return (
                usb_power_hub_using_dmc.UsbPowerHubUsingDmc(
                    device_name=fx_device.device_name,
                ),
                None,
            )

    async def _log_message_to_devices(
        self, message: str, level: custom_types.LEVEL
    ) -> None:
        """Log message in all the Fuchsia devices.

        Args:
            message: Message that need to logged.
            level: Log message level.
        """
        for fx_device in self.fuchsia_devices:
            try:
                await fx_device.log_message_to_device(message, level)
            except Exception as err:
                _LOGGER.exception(
                    "Unable to log message '%s' on '%s'. Failed with error: %s",
                    message,
                    fx_device.device_name,
                    err,
                )

    def _process_metric_user_params(self) -> None:
        _LOGGER.info(
            "user_params associated with the test: %s", self.user_params
        )

        snapshot_on: str = self.user_params.get(
            "snapshot_on", SnapshotOn.TEARDOWN_CLASS_ON_FAIL.value
        ).lower()
        tracing_on: str = self.user_params.get(
            "tracing_on", SnapshotOn.NEVER.value
        ).lower()
        self.trace_categories: list[str] = self.user_params.get(
            "trace_categories", None
        )

        try:
            self.snapshot_on: SnapshotOn = SnapshotOn(snapshot_on)
            self.tracing_on: TracingOn = TracingOn(tracing_on)
        except ValueError as e:
            raise signals.TestAbortClass("invalid metric user_param") from e


# LINT.ThenChange(//src/testing/end_to_end/mobly_base_tests/fuchsia_base_test/fuchsia_base_test.py)
