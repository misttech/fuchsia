# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from pydap.dap_types import Thread
from pydap.models import (
    ThreadEvent,
    ThreadEventBody,
    ThreadsResponse,
    ThreadsResponseBody,
)


class ZxdbThread(Thread):
    """Zxdb-specific thread model containing processId."""

    process_id: int | None = None


class ZxdbThreadsResponseBody(ThreadsResponseBody):
    """Body of response to `threads` request containing zxdb threads."""

    threads: list[ZxdbThread]


class ZxdbThreadsResponse(ThreadsResponse):
    """Response to `threads` request with zxdb specific thread info."""

    body: ZxdbThreadsResponseBody


class ZxdbThreadEventBody(ThreadEventBody):
    """Body of the thread event containing optional processId."""

    process_id: int | None = None


class ZxdbThreadEvent(ThreadEvent):
    """Zxdb-specific thread event with processId in body."""

    body: ZxdbThreadEventBody
