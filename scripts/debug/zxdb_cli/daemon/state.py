# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from dataclasses import dataclass, field


@dataclass
class Thread:
    """Represents a thread in a target process."""

    id: int
    name: str = field(default="", compare=False)
    is_stopped: bool = field(default=False, compare=False)
    process: "Process | None" = field(default=None, repr=False, compare=False)


@dataclass
class Process:
    """Represents a debugged target process."""

    id: int
    name: str = field(default="", compare=False)
    threads: dict[int, Thread] = field(
        default_factory=dict, repr=False, compare=False
    )
