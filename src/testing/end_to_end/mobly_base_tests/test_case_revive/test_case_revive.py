# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import contextlib
from typing import Any, Iterator

import test_case_revive as test_case_revive_pkg

FuchsiaDeviceOperation = test_case_revive_pkg.FuchsiaDeviceOperation

TestMethodExecutionFrequency = test_case_revive_pkg.TestMethodExecutionFrequency

opt_out = test_case_revive_pkg.opt_out

tag_test = test_case_revive_pkg.tag_test


class TestCaseRevive(test_case_revive_pkg.AsyncTestCaseRevive):
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

    def pre_run(self) -> None:  # type: ignore
        with self._async_devices():
            super().pre_run()  # type: ignore

    def _perform_op(  # type: ignore
        self, fuchsia_device_operation: FuchsiaDeviceOperation
    ) -> None:
        with self._async_devices():
            super()._perform_op(fuchsia_device_operation)

    def _logic_for_test_case_revive(  # type: ignore
        self,
        test_case: str,
        fuchsia_device_operation: FuchsiaDeviceOperation,
        test_method_execution_frequency: TestMethodExecutionFrequency,
        pre_test_execution_fn: Any | None,
        pre_test_execution_fn_kwargs: dict[str, Any] | None,
        post_test_execution_fn: Any | None,
        post_test_execution_fn_kwargs: dict[str, Any] | None,
    ) -> None:
        with self._async_devices():
            super()._logic_for_test_case_revive(
                test_case,
                fuchsia_device_operation,
                test_method_execution_frequency,
                pre_test_execution_fn,
                pre_test_execution_fn_kwargs,
                post_test_execution_fn,
                post_test_execution_fn_kwargs,
            )

    def _revived_test_case_name_func(  # type: ignore
        self,
        test_case: str,
        fuchsia_device_operation: FuchsiaDeviceOperation,
        test_method_execution_frequency: TestMethodExecutionFrequency,
        pre_test_execution_fn: Any | None,
        pre_test_execution_fn_kwargs: dict[str, Any] | None,
        post_test_execution_fn: Any | None,
        post_test_execution_fn_kwargs: dict[str, Any] | None,
    ) -> str:
        with self._async_devices():
            return super()._revived_test_case_name_func(
                test_case,
                fuchsia_device_operation,
                test_method_execution_frequency,
                pre_test_execution_fn,
                pre_test_execution_fn_kwargs,
                post_test_execution_fn,
                post_test_execution_fn_kwargs,
            )

    def _read_and_validate_user_params(self) -> None:  # type: ignore
        with self._async_devices():
            super()._read_and_validate_user_params()

    def _get_list_of_revived_test_cases(self) -> list[str]:  # type: ignore
        with self._async_devices():
            return super()._get_list_of_revived_test_cases()

    def _generate_test_args_tuple_list(  # type: ignore
        self, revived_test_cases: list[str]
    ) -> list[Any]:
        with self._async_devices():
            return super()._generate_test_args_tuple_list(revived_test_cases)

    def setup_class(self) -> None:  # type: ignore
        super().setup_class()  # type: ignore
        async_devices_result: list[Any] = self.fuchsia_devices
        self.fuchsia_devices = [
            device.as_sync() for device in async_devices_result
        ]

    def teardown_class(self) -> None:  # type: ignore
        with self._async_devices():
            super().teardown_class()  # type: ignore

    def setup_test(self) -> None:  # type: ignore
        with self._async_devices():
            super().setup_test()  # type: ignore

    def teardown_test(self) -> None:  # type: ignore
        with self._async_devices():
            super().teardown_test()  # type: ignore

    def on_fail(self, record) -> None:  # type: ignore
        with self._async_devices():
            super().on_fail(record)  # type: ignore

    def on_pass(self, record) -> None:  # type: ignore
        with self._async_devices():
            super().on_pass(record)  # type: ignore

    def on_skip(self, record) -> None:  # type: ignore
        with self._async_devices():
            super().on_skip(record)  # type: ignore
