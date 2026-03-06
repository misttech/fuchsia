# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Abstract base class for Bluetooth Gap Profile affordance."""
import abc

from honeydew.affordances import affordance
from honeydew.affordances.connectivity.bluetooth.bluetooth_common import (
    bluetooth_common,
)


class AsyncGap(bluetooth_common.AsyncBluetoothCommon):
    """Abstract base class for an async Bluetooth Gap Profile affordance."""


class Gap(affordance.Affordance, bluetooth_common.BluetoothCommon):
    """Abstract base class for Bluetooth Gap Profile affordance."""

    @abc.abstractmethod
    def as_async(self) -> AsyncGap:
        """Returns the async version of Gap."""
