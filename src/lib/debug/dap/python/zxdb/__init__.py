# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

from pydap.client import DapClient

from .zxdb_dap_mixin import (
    ZxdbDapMixin,
    ZxdbDetachArguments,
    ZxdbStackTraceArguments,
)


class ZxdbDapClient(ZxdbDapMixin, DapClient):
    pass


__all__ = [
    "ZxdbDapClient",
    "ZxdbDapMixin",
    "ZxdbDetachArguments",
    "ZxdbStackTraceArguments",
]
