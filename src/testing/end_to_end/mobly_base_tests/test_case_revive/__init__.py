# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import dataclasses
import importlib
import inspect
import logging
from collections.abc import Callable
from typing import Any

from fuchsia_base_test import AsyncFuchsiaBaseTest
from honeydew.auxiliary_devices.power_switch import power_switch

from .test_case_revive import (
    _DMC_CLASS,
    _DMC_MODULE,
    FuchsiaDeviceOperation,
    TestMethodExecutionFrequency,
    _TestArgTuple,
)

_LOGGER: logging.Logger = logging.getLogger(__name__)


class AsyncTestCaseRevive(AsyncFuchsiaBaseTest):
    """Async version of TestCaseRevive."""

    # LINT.IfChange(pre_run_async)
    async def pre_run(self) -> None:
        """Mobly method used to generate the test cases at run time."""
        await AsyncFuchsiaBaseTest.pre_run(self)

        self._test_case_revive: bool = self.user_params.get(
            "test_case_revive", False
        )

        if self._test_case_revive is False:
            _LOGGER.info(
                "[AsyncTestCaseRevive] - test_case_revive setting is not enabled "
                "in user_params. So not testing in revive mode...",
            )
            return

        _LOGGER.info(
            "[AsyncTestCaseRevive] - test_case_revive setting is enabled in "
            "user_params. So testing in revive mode...",
        )

        self._read_and_validate_user_params()

        revived_test_cases: list[str] = self._get_list_of_revived_test_cases()

        test_arg_tuple_list: list[
            _TestArgTuple
        ] = self._generate_test_args_tuple_list(revived_test_cases)

        self.generate_tests(
            test_logic=self._logic_for_test_case_revive,
            name_func=self._revived_test_case_name_func,
            arg_sets=[
                dataclasses.astuple(test_arg_tuple)
                for test_arg_tuple in test_arg_tuple_list
            ],
        )

    # LINT.ThenChange(:pre_run)

    async def _perform_op(
        self, fuchsia_device_operation: FuchsiaDeviceOperation
    ) -> None:
        """Perform user specified operation"""

        for fuchsia_device in self.fuchsia_devices:
            if (
                fuchsia_device_operation
                == FuchsiaDeviceOperation.IDLE_SUSPEND_TIMER_RESUME
            ):
                await fuchsia_device.system_power_state_controller.idle_suspend_timer_based_resume(
                    duration=self.user_params["resume_timer_duration_sec"],
                    verify_duration=self.user_params.get(
                        "verify_duration", True
                    ),
                )
            elif fuchsia_device_operation == FuchsiaDeviceOperation.SOFT_REBOOT:
                await fuchsia_device.reboot()
            elif fuchsia_device_operation in [
                FuchsiaDeviceOperation.HARD_REBOOT,
                FuchsiaDeviceOperation.POWER_CYCLE,
            ]:
                _LOGGER.debug(
                    "[AsyncTestCaseRevive] - Importing %s.%s module",
                    _DMC_MODULE,
                    _DMC_CLASS,
                )
                power_switch_class: type[power_switch.PowerSwitch] = getattr(
                    importlib.import_module(_DMC_MODULE), _DMC_CLASS
                )

                _LOGGER.debug(
                    "[AsyncTestCaseRevive] - Instantiating %s.%s module",
                    _DMC_MODULE,
                    _DMC_CLASS,
                )
                self._power_switch: power_switch.PowerSwitch = (
                    power_switch_class(device_name=fuchsia_device.device_name)
                )

                await fuchsia_device.power_cycle(
                    power_switch=self._power_switch, outlet=None
                )
            elif (
                fuchsia_device_operation
                == FuchsiaDeviceOperation.IDLE_SUSPEND_BUTTON_PRESS_RESUME
            ):
                raise ValueError(
                    f"'{fuchsia_device_operation}' operation is not supported by 'AsyncTestCaseRevive'"
                )

    # LINT.IfChange(logic_for_test_case_revive_async)
    async def _logic_for_test_case_revive(
        self,
        test_case: str,
        fuchsia_device_operation: FuchsiaDeviceOperation,
        test_method_execution_frequency: TestMethodExecutionFrequency,
        pre_test_execution_fn: Callable[[], Any] | None,
        pre_test_execution_fn_kwargs: dict[str, Any] | None,
        post_test_execution_fn: Callable[[], Any] | None,
        post_test_execution_fn_kwargs: dict[str, Any] | None,
    ) -> None:
        """AsyncTestCaseRevive logic"""

        sequence: str
        if (
            test_method_execution_frequency
            == TestMethodExecutionFrequency.PRE_AND_POST
        ):
            sequence = (
                f"{test_case} -> {fuchsia_device_operation} -> {test_case}"
            )
        elif (
            test_method_execution_frequency
            == TestMethodExecutionFrequency.PRE_ONLY
        ):
            sequence = f"{test_case} -> {fuchsia_device_operation}"
        else:  # TestMethodExecutionFrequency.POST_ONLY
            sequence = f"{fuchsia_device_operation} -> {test_case}"
        if pre_test_execution_fn:
            sequence = f"{pre_test_execution_fn.__qualname__} -> {sequence}"
        if post_test_execution_fn:
            sequence = f"{sequence} -> {post_test_execution_fn.__qualname__}"
        _LOGGER.info("[AsyncTestCaseRevive] - Revived test logic: %s", sequence)

        if pre_test_execution_fn:
            if pre_test_execution_fn_kwargs:
                _LOGGER.info(
                    "[AsyncTestCaseRevive] - Calling %s with args:%s which is the first step in the revived test sequence...",
                    pre_test_execution_fn.__qualname__,
                    pre_test_execution_fn_kwargs,
                )
                res = pre_test_execution_fn(**pre_test_execution_fn_kwargs)
                if inspect.iscoroutine(res):
                    await res
            else:
                _LOGGER.info(
                    "[AsyncTestCaseRevive] - Calling %s which is the first step in the revived test sequence...",
                    pre_test_execution_fn.__qualname__,
                )
                res = pre_test_execution_fn()
                if inspect.iscoroutine(res):
                    await res

        if test_method_execution_frequency in [
            TestMethodExecutionFrequency.PRE_AND_POST,
            TestMethodExecutionFrequency.PRE_ONLY,
        ]:
            _LOGGER.info(
                "[AsyncTestCaseRevive] - Running the %s before performing %s operation...",
                test_case,
                fuchsia_device_operation,
            )
            res = getattr(self, test_case)()
            if inspect.iscoroutine(res):
                await res

        _LOGGER.info(
            "[AsyncTestCaseRevive] - Performing %s operation on all Fuchsia devices "
            "that are part of the testbed...",
            fuchsia_device_operation,
        )
        await self._perform_op(
            fuchsia_device_operation=fuchsia_device_operation
        )

        if test_method_execution_frequency in [
            TestMethodExecutionFrequency.PRE_AND_POST,
            TestMethodExecutionFrequency.POST_ONLY,
        ]:
            _LOGGER.info(
                "[AsyncTestCaseRevive] - Running the %s after performing %s operation...",
                test_case,
                fuchsia_device_operation,
            )
            res = getattr(self, test_case)()
            if inspect.iscoroutine(res):
                await res

        if post_test_execution_fn:
            if post_test_execution_fn_kwargs:
                _LOGGER.info(
                    "[AsyncTestCaseRevive] - Calling %s with args:%s which is the last step in the revived test sequence...",
                    post_test_execution_fn.__qualname__,
                    post_test_execution_fn_kwargs,
                )
                res = post_test_execution_fn(**post_test_execution_fn_kwargs)
                if inspect.iscoroutine(res):
                    await res
            else:
                _LOGGER.info(
                    "[AsyncTestCaseRevive] - Calling %s which is the last step in the revived test sequence...",
                    post_test_execution_fn.__qualname__,
                )
                res = post_test_execution_fn()
                if inspect.iscoroutine(res):
                    await res

    # LINT.ThenChange(:logic_for_test_case_revive)

    def _revived_test_case_name_func(
        self,
        test_case: str,
        fuchsia_device_operation: FuchsiaDeviceOperation,
        test_method_execution_frequency: TestMethodExecutionFrequency,
        pre_test_execution_fn: Callable[[], None] | None,
        pre_test_execution_fn_kwargs: dict[str, Any] | None,
        post_test_execution_fn: Callable[[], None] | None,
        post_test_execution_fn_kwargs: dict[str, Any] | None,
    ) -> str:
        """Revived test case name function"""
        test_case = test_case.lstrip("_")
        return f"{test_case}_revived_with_{fuchsia_device_operation}"

    def _read_and_validate_user_params(self) -> None:
        """Read the user params associated with this test"""
        self._fuchsia_device_operation: str = self.user_params.get(
            "fuchsia_device_operation", FuchsiaDeviceOperation.NONE
        )
        try:
            self._fuchsia_device_operation_obj: FuchsiaDeviceOperation = (
                FuchsiaDeviceOperation(self._fuchsia_device_operation.lower())
            )
        except ValueError as err:
            raise ValueError(
                f"'{self._fuchsia_device_operation}' operation is not "
                f"supported by 'AsyncTestCaseRevive'"
            ) from err

        if (
            self._fuchsia_device_operation_obj
            == FuchsiaDeviceOperation.IDLE_SUSPEND_TIMER_RESUME
        ):
            if not isinstance(
                self.user_params.get("resume_timer_duration_sec"), int
            ):
                raise ValueError(
                    f"FuchsiaDeviceOperation: '{self._fuchsia_device_operation}' "
                    f"requires user to also pass 'resume_timer_duration_sec' key "
                    f"with int value in test prams"
                )

        self._test_method_execution_frequency: str = self.user_params.get(
            "test_method_execution_frequency",
            TestMethodExecutionFrequency.PRE_AND_POST,
        )
        try:
            self._test_method_execution_frequency_obj: (
                TestMethodExecutionFrequency
            ) = TestMethodExecutionFrequency(
                self._test_method_execution_frequency
            )
        except ValueError as err:
            raise ValueError(
                f"'{self._test_method_execution_frequency}' is not valid"
            ) from err

    def _get_list_of_revived_test_cases(self) -> list[str]:
        """Return the list of test cases that need to be revived."""
        test_cases: list[str] = [
            attribute
            for attribute in dir(self)
            if attribute.startswith(("test_", "_test_")) is True
            and callable(getattr(self, attribute))
        ]
        _LOGGER.info(
            "[AsyncTestCaseRevive] - List of all the test cases in this test "
            "class: %s",
            test_cases,
        )

        revived_test_cases = [
            test_case
            for test_case in test_cases
            if "_opt_out" not in dir(getattr(self, test_case))
            or not getattr(self, test_case)._opt_out
        ]
        _LOGGER.info(
            "[AsyncTestCaseRevive] - List of all the test cases in this test class "
            "that are configured to run with revived sequence: %s",
            revived_test_cases,
        )

        return revived_test_cases

    def _generate_test_args_tuple_list(
        self, revived_test_cases: list[str]
    ) -> list[_TestArgTuple]:
        """Generate the list of duple data structure that need to be passed to
        Mobly's generate_tests() method."""

        test_arg_tuple_list: list[_TestArgTuple] = []
        for revived_test_case in revived_test_cases:
            fuchsia_device_operation: FuchsiaDeviceOperation = (
                self._fuchsia_device_operation_obj
            )
            if "_fuchsia_device_operation" in dir(
                getattr(self, revived_test_case)
            ):
                fuchsia_device_operation = getattr(
                    self, revived_test_case
                )._fuchsia_device_operation

            test_method_execution_frequency: TestMethodExecutionFrequency = (
                self._test_method_execution_frequency_obj
            )
            if "_test_method_execution_frequency" in dir(
                getattr(self, revived_test_case)
            ):
                test_method_execution_frequency = getattr(
                    self, revived_test_case
                )._test_method_execution_frequency

            pre_test_execution_fn: Callable[[], None] | None = None
            if "_pre_test_execution_fn" in dir(
                getattr(self, revived_test_case)
            ):
                pre_test_execution_fn = getattr(
                    self, revived_test_case
                )._pre_test_execution_fn

            pre_test_execution_fn_kwargs: dict[str, Any] | None = None
            if "_pre_test_execution_fn_kwargs" in dir(
                getattr(self, revived_test_case)
            ):
                pre_test_execution_fn_kwargs = getattr(
                    self, revived_test_case
                )._pre_test_execution_fn_kwargs

            post_test_execution_fn: Callable[[], None] | None = None
            if "_post_test_execution_fn" in dir(
                getattr(self, revived_test_case)
            ):
                post_test_execution_fn = getattr(
                    self, revived_test_case
                )._post_test_execution_fn

            post_test_execution_fn_kwargs: dict[str, Any] | None = None
            if "_post_test_execution_fn_kwargs" in dir(
                getattr(self, revived_test_case)
            ):
                post_test_execution_fn_kwargs = getattr(
                    self, revived_test_case
                )._post_test_execution_fn_kwargs

            test_arg_tuple: _TestArgTuple = _TestArgTuple(
                test_case_method=revived_test_case,
                fuchsia_device_operation=fuchsia_device_operation,
                test_method_execution_frequency=test_method_execution_frequency,
                pre_test_execution_fn=pre_test_execution_fn,
                pre_test_execution_fn_kwargs=pre_test_execution_fn_kwargs,
                post_test_execution_fn=post_test_execution_fn,
                post_test_execution_fn_kwargs=post_test_execution_fn_kwargs,
            )

            test_arg_tuple_list.append(test_arg_tuple)

        _LOGGER.debug(
            "[AsyncTestCaseRevive] - Test arg list that will be passed to Mobly's "
            "generate_test method to generate the revived test cases is: %s",
            test_arg_tuple_list,
        )

        return test_arg_tuple_list


from .test_case_revive import TestCaseRevive, opt_out, tag_test
