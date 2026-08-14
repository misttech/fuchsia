# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from pydap.client import DapClient
from pydap.models import ThreadEvent, ThreadEventBody
from zxdb_dap.models import (
    ZxdbPauseArguments,
    ZxdbProcessStoppedEvent,
    ZxdbProcessStoppedEventBody,
    ZxdbThread,
    ZxdbThreadEvent,
    ZxdbThreadEventBody,
    ZxdbThreadsResponse,
    ZxdbThreadsResponseBody,
)
from zxdb_dap.zxdb_dap_mixin import (
    ZxdbDapMixin,
    ZxdbDetachArguments,
    ZxdbProcessArguments,
    ZxdbProcessInfo,
    ZxdbProcessResponse,
    ZxdbProcessResponseBody,
    ZxdbStackTraceArguments,
)


class ZxdbDapClient(ZxdbDapMixin, DapClient):
    pass


__all__ = [
    "ZxdbDapClient",
    "ZxdbDapMixin",
    "ZxdbDetachArguments",
    "ZxdbPauseArguments",
    "ZxdbProcessArguments",
    "ZxdbProcessInfo",
    "ZxdbProcessResponse",
    "ZxdbProcessResponseBody",
    "ZxdbProcessStoppedEvent",
    "ZxdbProcessStoppedEventBody",
    "ZxdbStackTraceArguments",
    "ThreadEvent",
    "ZxdbThread",
    "ZxdbThreadEvent",
    "ZxdbThreadEventBody",
    "ZxdbThreadsResponse",
    "ZxdbThreadsResponseBody",
]
