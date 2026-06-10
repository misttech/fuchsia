# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from typing import Literal

from shared.protocol.base import BaseRequest


class HelloRequest(BaseRequest):
    """Initial handshake request to verify protocol version."""

    command: Literal["hello"] = "hello"
    version: int
