# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Utilities for power-related operations on Fuchsia devices."""

import dataclasses
import logging
from datetime import timedelta

from mobly import asserts

from honeydew import errors
from honeydew.fuchsia_device import fuchsia_device
from honeydew.transports.ffx import types as ffx_types
from honeydew.utils import control_flows

_LOGGER: logging.Logger = logging.getLogger(__name__)

# How long to wait before we give up on suspending. It's a tradeoff between
# deflaking and worst-case test duration.
SUSPEND_RESUME_DEFAULT_TIMEOUT: timedelta = timedelta(minutes=5)

# This is the minimum time we'll idle waiting for the device to suspend. If it
# doesn't suspend in time, we'll double the waiting period until we
# successfully suspend, or the deadline runs out.
SUSPEND_RESUME_BASE_IDLE_DURATION: timedelta = timedelta(seconds=5)


@dataclasses.dataclass
class SagSuspendStats:
    """Aggregate stats about suspend, exposed by SAG via inspect."""

    success_count: int
    fail_count: int
    total_time_in_suspend: timedelta

    def __sub__(self, other: "SagSuspendStats") -> "SagSuspendStats":
        return SagSuspendStats(
            success_count=self.success_count - other.success_count,
            fail_count=self.fail_count - other.fail_count,
            total_time_in_suspend=self.total_time_in_suspend
            - other.total_time_in_suspend,
        )


def get_sag_suspend_stats(
    device: fuchsia_device.FuchsiaDevice,
) -> SagSuspendStats:
    """Returns the aggregate stats about suspend, exposed by SAG via inspect.

    Args:
        device: Fuchsia device object.

    Returns:
        SagSuspendStats: Aggregate stats about suspend.

    Raises:
        errors.InspectError: If SAG inspect data is missing.
    """
    selectors = ["bootstrap/system-activity-governor:root"]
    inspect_data_collection = device.get_inspect_data(selectors=selectors)

    if not inspect_data_collection.data:
        raise errors.InspectError(
            f"SAG inspect data associated with {device.device_name} is empty"
        )

    sag_inspect_data = inspect_data_collection.data[0]

    if sag_inspect_data.payload is None:
        raise errors.InspectError(
            f"SAG inspect data associated with {device.device_name} does "
            "not have a valid payload"
        )

    try:
        stats = sag_inspect_data.payload["root"]["suspend_stats"]
        return SagSuspendStats(
            success_count=stats["success_count"],
            fail_count=stats["fail_count"],
            total_time_in_suspend=timedelta(
                microseconds=stats["total_time_in_suspend_ns"] / 1000
            ),
        )
    except KeyError as err:
        raise errors.InspectError(
            f"SAG inspect data associated with {device.device_name} is "
            f"missing expected field: {err}"
        ) from err


def suspend_resume(
    device: fuchsia_device.FuchsiaDevice,
    deadline: control_flows.Deadline | None = None,
) -> None:
    """Disconnects USB, idles, reconnects.

    Args:
        deadline: this will idle for increasing durations, up to this deadline.
    """
    if deadline is None:
        deadline = control_flows.Deadline.from_duration(
            SUSPEND_RESUME_DEFAULT_TIMEOUT
        )

    try:
        device.ffx.run(
            ["session", "drop-power-lease"],
            machine=ffx_types.MachineFormat.DISABLE,
        )
    except Exception as e:
        # TODO(https://fxbug.dev/485577846): Don't swallow this error.
        # We should have a way to
        # drop the power lease even if it's already missing, e.g.,
        # `ffx session drop-power-lease --allow-missing`.
        _LOGGER.warning(f"Failed to drop power lease: {e}")

    attempt = -1
    while True:
        attempt += 1

        if deadline.is_due():
            asserts.fail("SAG did not suspend during idle.")

        _LOGGER.info(f"Suspension attempt {attempt + 1}...")
        before_off_charger_stats = get_sag_suspend_stats(device)

        sleep_deadline = deadline.subdeadline_from_duration(
            SUSPEND_RESUME_BASE_IDLE_DURATION * (2**attempt)
        )
        try:
            device.suspend()
            control_flows.sleep_for_duration(
                sleep_deadline.remaining_duration()
            )
        finally:
            device.resume()

        while_off_charger_stats = (
            get_sag_suspend_stats(device) - before_off_charger_stats
        )

        _LOGGER.info(
            f"Suspend stats during off-charger idle: \n{while_off_charger_stats}"
        )

        # Ensure we actually suspended.
        if while_off_charger_stats.success_count == 0:
            _LOGGER.warning("SAG did not suspend during idle. Retrying...")
            continue

        # If we get here, we successfully suspended.
        return
