# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest

from dap_test_framework import DapTestCase
from pydap.models import (
    EvaluateArguments,
    InitializeArguments,
    LaunchArguments,
    StackTraceArguments,
)


class TestDapSmoke(DapTestCase):
    async def test_smoke_flow(self) -> None:
        target = "fuchsia-pkg://fuchsia.com/zxdb_e2e_inferiors#meta/rust_functions.cm"

        await self.initialize(InitializeArguments(adapter_id="zxdb"))
        await self.on_event("initialized")

        await self.evaluate(EvaluateArguments(expression="b $main"))
        await self.launch(LaunchArguments(process=target))
        await self.on_event("stopped").expect(
            {"body": {"reason": "breakpoint"}}
        )

        threads_resp = await self.threads().expect(
            {"body": {"threads": [{"name": "initial-thread"}]}}
        )
        thread_id = threads_resp["body"]["threads"][0]["id"]

        self.stack_trace(StackTraceArguments(thread_id=thread_id)).expect(
            {"body": {"stackFrames": [...]}}
        )

        await self.verify_all_expectations()


if __name__ == "__main__":
    unittest.main()
