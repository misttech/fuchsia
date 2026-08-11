# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Abstract base class for location affordance."""

import abc

from honeydew.affordances import affordance
from honeydew.affordances.connectivity.wlan.utils.types import (
    CountryCode,
)


class AsyncLocation(abc.ABC):
    """Abstract base class for an async Location affordance."""

    @abc.abstractmethod
    async def set_region(self, region_code: CountryCode) -> None:
        """Set regulatory region.

        Args:
            region_code: 2-byte country code.

        Raises:
            HoneydewLocationError: Error from location stack
            TypeError: Invalid region_code format
        """


class Location(affordance.Affordance):
    """Abstract base class for Location affordance."""

    # List all the public methods
    @abc.abstractmethod
    def set_region(self, region_code: CountryCode) -> None:
        """Set regulatory region.

        Args:
            region_code: 2-byte country code.

        Raises:
            HoneydewLocationError: Error from location stack
            TypeError: Invalid region_code format
        """

    @abc.abstractmethod
    def as_async(self) -> AsyncLocation:
        """Returns the async version of Location."""
