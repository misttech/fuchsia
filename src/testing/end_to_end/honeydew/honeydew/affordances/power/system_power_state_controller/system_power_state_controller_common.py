# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Common base class for SystemPowerStateController affordance implementations."""

import abc
import contextlib
import logging
from collections.abc import AsyncGenerator
from typing import Any

import fuchsia_inspect
from honeydew import affordances_capable, errors
from honeydew.affordances.power.system_power_state_controller import (
    system_power_state_controller as system_power_state_controller_interface,
)
from honeydew.transports.ffx import ffx as ffx_transport
from honeydew.typing import custom_types

_LOGGER: logging.Logger = logging.getLogger(__name__)


class SystemPowerStateControllerCommon(
    system_power_state_controller_interface.SystemPowerStateController
):
    """Common base class for async SystemPowerStateController implementations.

    Args:
        device_name: Device name returned by `ffx target list`.
        ffx: FFX transport implementation.
        device_logger: FuchsiaDeviceLogger implementation.
        inspect: InspectCapableDevice implementation.
    """

    _RESUME_DURATION_TOLERANCE_MILLISECONDS: float = 200

    def __init__(
        self,
        device_name: str,
        ffx: ffx_transport.FFX,
        device_logger: affordances_capable.FuchsiaDeviceLogger,
        inspect: affordances_capable.InspectCapableDevice,
    ) -> None:
        self._device_name: str = device_name
        self._ffx: ffx_transport.FFX = ffx
        self._device_logger: affordances_capable.FuchsiaDeviceLogger = (
            device_logger
        )
        self._inspect: affordances_capable.InspectCapableDevice = inspect

    async def suspend_resume(
        self,
        suspend_state: system_power_state_controller_interface.SuspendState,
        resume_mode: system_power_state_controller_interface.ResumeMode,
    ) -> None:
        """Perform suspend-resume operation on the device.

        This is a synchronous operation on the device and thus this call will
        hang until resume operation finishes.

        Args:
            suspend_state: Which state to suspend the Fuchsia device into.
            resume_mode: Information about how to resume the device.

        Raises:
            SystemPowerStateControllerError: In case of failure.
            errors.NotSupportedError: If any of the suspend_state or resume_mode
                is not yet supported.
            ValueError: If any of the input args are not valid.
        """
        self._validate_suspend_resume_method_args(
            suspend_state=suspend_state,
            resume_mode=resume_mode,
        )

        async with self._validate_suspend_resume_using_inspect(
            suspend_state=suspend_state,
            resume_mode=resume_mode,
        ):
            log_message: str = (
                f"Performing '{suspend_state}' followed by '{resume_mode}' "
                f"operations on '{self._device_name}'..."
            )
            _LOGGER.info(log_message)
            await self._device_logger.log_message_to_device(
                message=log_message,
                level=custom_types.LEVEL.INFO,
            )

            await self._perform_suspend_resume(
                suspend_state=suspend_state,
                resume_mode=resume_mode,
            )

            log_message = (
                f"Completed '{suspend_state}' followed by '{resume_mode}' "
                f"operations on '{self._device_name}'."
            )
            await self._device_logger.log_message_to_device(
                message=log_message,
                level=custom_types.LEVEL.INFO,
            )
            _LOGGER.info(log_message)

    async def idle_suspend_timer_based_resume(
        self,
        duration: int,
        verify_duration: bool = True,
    ) -> None:
        """Perform idle-suspend and timer-based-resume operation on the device.

        Args:
            duration: Resume timer duration in seconds.
            verify_duration: If set to True, verifies suspend-resume operation completed within the
                duration specified. If set to False, skips this verification. Default is True.

        Raises:
            SystemPowerStateControllerError: In case of failure.
            ValueError: If any of the input args are not valid.
        """
        await self.suspend_resume(
            suspend_state=system_power_state_controller_interface.IdleSuspend(),
            resume_mode=system_power_state_controller_interface.TimerResume(
                duration=duration,
                verify_duration=verify_duration,
            ),
        )

    @abc.abstractmethod
    async def _perform_suspend_resume(
        self,
        suspend_state: system_power_state_controller_interface.SuspendState,
        resume_mode: system_power_state_controller_interface.ResumeMode,
    ) -> None:
        """Perform the implementation-specific suspend and resume operations.

        Args:
            suspend_state: Which state to suspend the Fuchsia device into.
            resume_mode: Information about how to resume the device.

        Raises:
            SystemPowerStateControllerError: In case of failure.
        """

    def _validate_suspend_resume_method_args(
        self,
        suspend_state: system_power_state_controller_interface.SuspendState,
        resume_mode: system_power_state_controller_interface.ResumeMode,
    ) -> None:
        """Validate the input args of suspend_resume() method.

        Args:
            suspend_state: Which state to suspend the Fuchsia device into.
            resume_mode: Information about how to resume the device.

        Raises:
            errors.NotSupportedError: If any of the suspend_state or resume_mode
                is not yet supported.
            ValueError: If any of the input args are not valid.
        """
        if not isinstance(
            suspend_state,
            (system_power_state_controller_interface.IdleSuspend,),
        ):
            raise errors.NotSupportedError(
                f"Suspending the device to '{suspend_state}' state is not yet "
                f"supported."
            )

        if not isinstance(
            resume_mode,
            (system_power_state_controller_interface.TimerResume,),
        ):
            raise errors.NotSupportedError(
                f"Resuming the device using '{resume_mode}' is not yet supported."
            )

        if resume_mode.duration <= 0:
            raise ValueError(
                f"Resume timer duration must be positive, got {resume_mode.duration}."
            )

    @contextlib.asynccontextmanager
    async def _validate_suspend_resume_using_inspect(
        self,
        suspend_state: system_power_state_controller_interface.SuspendState,
        resume_mode: system_power_state_controller_interface.ResumeMode,
    ) -> AsyncGenerator[None, None]:
        """Validate suspend-resume operation using SAG inspect data.

        Args:
            suspend_state: Which state to suspend the Fuchsia device into.
            resume_mode: Information about how to resume the device.

        Raises:
            SystemPowerStateControllerError: In case of failure.
        """
        suspend_resume_stats_before: dict[
            str, int
        ] = await self._get_suspend_stats_from_sag_inspect_data()

        yield

        suspend_resume_stats_after: dict[
            str, int
        ] = await self._get_suspend_stats_from_sag_inspect_data()
        self._validate_using_sag_inspect_data(
            suspend_state,
            resume_mode,
            suspend_resume_stats_before,
            suspend_resume_stats_after,
        )

    def _validate_using_sag_inspect_data(
        self,
        suspend_state: system_power_state_controller_interface.SuspendState,
        resume_mode: system_power_state_controller_interface.ResumeMode,
        suspend_resume_stats_before: dict[str, int],
        suspend_resume_stats_after: dict[str, int],
    ) -> None:
        """Validate suspend-resume operation using SAG inspect data.

        Args:
            suspend_state: Which state to suspend the Fuchsia device into.
            resume_mode: Information about how to resume the device.
            suspend_resume_stats_before: Suspend-Resume stats before.
            suspend_resume_stats_after: Suspend-Resume stats after.

        Raises:
            SystemPowerStateControllerError: In case of failure.
        """
        # "suspend_stats": {
        #     "fail_count": 0,
        #     "last_failed_error": 0,
        #     "last_time_in_suspend_ns": 3053743875,
        #     "last_time_in_suspend_operations": 188959,
        #     "success_count": 1
        # },

        if (
            suspend_resume_stats_after["success_count"]
            <= suspend_resume_stats_before["success_count"]
        ):
            raise system_power_state_controller_interface.SystemPowerStateControllerError(
                f"Based on SAG inspect data, '{suspend_state}' followed "
                f"by '{resume_mode}' operation didn't succeed on "
                f"'{self._device_name}', expected "
                f"{suspend_resume_stats_before['success_count'] + 1}, "
                f"but found {suspend_resume_stats_after['success_count']}."
            )

        suspend_resume_duration_nano_sec: float = suspend_resume_stats_after[
            "last_time_in_suspend_ns"
        ]
        suspend_resume_duration_sec: float = round(
            suspend_resume_duration_nano_sec / 1e9, 4
        )

        self._validate_suspend_resume_duration_using_inspect_data(
            suspend_state=suspend_state,
            resume_mode=resume_mode,
            actual_suspend_resume_duration_nano_sec=suspend_resume_duration_nano_sec,
            inspect_data_src="SAG",
        )

        _LOGGER.info(
            "Successfully verified '%s' followed by '%s' operations on '%s' "
            "using SAG inspect data. Operations took '%s seconds' to complete.",
            suspend_state,
            resume_mode,
            self._device_name,
            suspend_resume_duration_sec,
        )

    def _validate_suspend_resume_duration_using_inspect_data(
        self,
        suspend_state: system_power_state_controller_interface.SuspendState,
        resume_mode: system_power_state_controller_interface.ResumeMode,
        actual_suspend_resume_duration_nano_sec: float,
        inspect_data_src: str,
    ) -> None:
        """Validate suspend-resume duration received from inspect data.

        Args:
            suspend_state: Which state to suspend the Fuchsia device into.
            resume_mode: Information about how to resume the device.
            actual_suspend_resume_duration_nano_sec: Suspend-Resume duration.
            inspect_data_src: Inspect data source.

        Raises:
            SystemPowerStateControllerError: In case of failure.
        """
        if not isinstance(
            resume_mode,
            (system_power_state_controller_interface.TimerResume,),
        ):
            return

        if resume_mode.verify_duration is False:
            return

        # It could take a few milliseconds for the device to fully resume after the timer fires.
        resume_duration_tolerance_nano_sec: float = (
            self._RESUME_DURATION_TOLERANCE_MILLISECONDS * 1e6
        )
        expected_suspend_resume_duration_with_tolerance_nano_sec: float = (
            resume_mode.duration * 1e9
        ) + resume_duration_tolerance_nano_sec

        if (
            actual_suspend_resume_duration_nano_sec
            > expected_suspend_resume_duration_with_tolerance_nano_sec
        ):
            actual_suspend_resume_duration_sec: float = round(
                actual_suspend_resume_duration_nano_sec / 1e9, 4
            )

            raise system_power_state_controller_interface.SystemPowerStateControllerError(
                f"Based on {inspect_data_src} inspect data, '{suspend_state}' followed by "
                f"'{resume_mode}' operation took "
                f"'{actual_suspend_resume_duration_sec} seconds' on "
                f"'{self._device_name}'. Expected it to not take more than "
                f"'{resume_mode.duration} seconds'.",
            )

    # pylint: disable=missing-raises-doc
    async def _get_suspend_stats_from_sag_inspect_data(self) -> dict[str, int]:
        """Returns suspend-resume stats by reading the SAG inspect data.

        Returns:
            suspend-resume stats.

        Raises:
            SystemPowerStateControllerError: Failed to read SAG inspect data.
        """
        try:
            inspect_data_collection: fuchsia_inspect.InspectDataCollection = (
                self._inspect.get_inspect_data(
                    selectors=["/bootstrap/system-activity-governor"],
                )
            )
            _LOGGER.debug(
                "SAG Inspect data associated with '%s' is: %s",
                self._device_name,
                inspect_data_collection,
            )

            if len(inspect_data_collection.data) == 0:
                raise errors.InspectError(
                    f"SAG inspect data associated with {self._device_name} is empty"
                )

            sag_inspect_data: fuchsia_inspect.InspectData = (
                inspect_data_collection.data[0]
            )

            if sag_inspect_data.payload is None:
                raise errors.InspectError(
                    f"SAG inspect data associated with {self._device_name} does "
                    f"not have a valid payload"
                )

            sag_inspect_root_data: dict[str, Any] = sag_inspect_data.payload[
                "root"
            ]
            return sag_inspect_root_data["suspend_stats"]
        except errors.InspectError as err:
            raise system_power_state_controller_interface.SystemPowerStateControllerError(
                f"Failed to read SAG inspect data from {self._device_name}"
            ) from err
