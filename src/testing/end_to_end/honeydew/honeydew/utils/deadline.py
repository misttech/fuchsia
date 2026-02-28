# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A more ergonomic Deadline type.

# Why not `datetime`?

The `datetime` module has a number of footguns that make it unsuitable for use
as a deadline type in tests:

1. It's easy to create a timezone-naive datetime, and creating timezone-aware
   datetimes is verbose.
2. `datetime` does not have a built-in way to represent an infinite deadline.
   `datetime.min` and `datetime.max` exist, but don't behave correctly with
   respect to arithmetic.
3. Infinite deadlines tend to be represented by `None`, which can be confused
   with a "default" deadline.
"""

import logging
from datetime import datetime, timedelta

_LOGGER: logging.Logger = logging.getLogger(__name__)


class Deadline:
    """Holds a deadline timestamp."""

    start: datetime
    deadline: datetime

    def __init__(self, start_time: datetime, deadline: datetime) -> None:
        """Creates a Deadline specifying start_time and deadline manually."""
        self.start = start_time
        self.deadline = deadline

    @staticmethod
    def from_duration(duration: timedelta) -> "Deadline":
        """Creates a Deadline instance based on a duration and the current timestamp"""
        now = datetime.now()
        return Deadline(now, now + duration)

    @staticmethod
    def from_datetime(deadline: datetime) -> "Deadline":
        """Creates a Deadline instance based on a deadline and the current timestamp"""
        return Deadline(datetime.now(), deadline)

    def subdeadline_from_duration(self, duration: timedelta) -> "Deadline":
        """Creates a new deadline that expires no later than `self`."""
        now = datetime.now()
        return Deadline(now, min(self.deadline, now + duration))

    def subdeadline_from_datetime(self, deadline: datetime) -> "Deadline":
        """Creates a new deadline that expires no later than `self`."""
        return Deadline(datetime.now(), min(self.deadline, deadline))

    def total_duration(self) -> timedelta:
        """Returns the total duration assigned to this deadline."""
        return self.deadline - self.start

    def is_due(self) -> bool:
        """Returns True if the deadline has passed, False otherwise."""
        return datetime.now() >= self.deadline

    def elapsed_duration(self) -> timedelta:
        """Returns the duration that has elapsed since the start time."""
        return datetime.now() - self.start

    def remaining_duration(self) -> timedelta:
        """Returns the duration remaining until the deadline."""
        return max(timedelta(seconds=0), self.deadline - datetime.now())

    def __str__(self) -> str:
        return f"Deadline(duration={self.total_duration()}, remaining={self.remaining_duration()})"
