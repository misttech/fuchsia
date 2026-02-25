# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import asyncio

_ASYNC_EVENT_LOOP: asyncio.AbstractEventLoop = asyncio.new_event_loop()
_ASYNC_EVENT_LOOP._name = "fuchsia_async_extension loop"  # type: ignore[attr-defined]


def get_loop() -> asyncio.AbstractEventLoop:
    return _ASYNC_EVENT_LOOP
