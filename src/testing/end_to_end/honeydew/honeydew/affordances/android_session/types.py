# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Data types used by AndroidSession affordance."""

import dataclasses


@dataclasses.dataclass(frozen=True)
class SessionId:
    """Android session ID based on critical system process PIDs."""

    system_server_pid: int
