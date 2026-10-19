# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for Tracing affordance."""

import logging
import os
import tempfile
import time

import fuchsia_base_test
from honeydew.affordances.tracing import tracing_using_ffx
from mobly import asserts, test_runner
from parameterized import parameterized
from trace_processing import trace_importing

_LOGGER: logging.Logger = logging.getLogger(__name__)


class TracingAffordanceTests(fuchsia_base_test.FuchsiaBaseTest):
    """Tracing affordance tests"""

    async def teardown_test(self) -> None:
        """teardown_test is called once after running each test.

        It does the following things:
            * Takes snapshot of all the fuchsia devices and stores it under
              test case directory if `snapshot_on` test param is set to
              "teardown_test"
            * Logs a info message onto device that test case has ended.
        """
        try:
            # in case if any trace session started by the test cases remains initialized.
            await self.dut.tracing.terminate()
        finally:
            await super().teardown_test()

    # Mobly enumerates test cases alphabetically, change in order of test cases
    # or their names or mobly enumeration logic can break tests. To avoid this,
    # we call all dependent operations in a single test method.
    async def test_tracing_terminate(self) -> None:
        """Test case for all tracing methods.

        This test case calls the following tracing methods:
                * `tracing.initialize()`
                * `tracing.is_session_initialized()`
                * `tracing.is_active()`
                * `tracing.start()`
                * `tracing.stop()`
                * `tracing.terminate()`
        """
        # Initialize Tracing Session.
        self.dut.tracing.initialize()
        asserts.assert_true(
            self.dut.tracing.is_session_initialized(),
            msg="Expected trace session to be initialized",
        )
        asserts.assert_false(
            self.dut.tracing.is_active(),
            msg="Expected trace session to be inactive before start()",
        )

        # Start Tracing.
        await self.dut.tracing.start()
        asserts.assert_true(
            self.dut.tracing.is_active(),
            msg="Expected trace session to be active after start()",
        )

        # Stop Tracing.
        await self.dut.tracing.stop()
        asserts.assert_true(
            self.dut.tracing.is_session_initialized(),
            msg="Expected trace session to remain initialized after stop()",
        )
        asserts.assert_false(
            self.dut.tracing.is_active(),
            msg="Expected trace session to be inactive after stop()",
        )

        # Terminate the tracing session.
        await self.dut.tracing.terminate()
        asserts.assert_false(
            self.dut.tracing.is_session_initialized(),
            msg="Expected trace session to be uninitialized after terminate()",
        )
        asserts.assert_false(
            self.dut.tracing.is_active(),
            msg="Expected trace session to be inactive after terminate()",
        )

    @parameterized.expand(
        [(False,), (True,)],
        name_func=lambda func, num, p: (
            f"{func.__name__}_{'compressed' if p.args[0] else 'uncompressed'}"
        ),
    )
    async def test_tracing_trace_download(self, compression: bool) -> None:
        """This test case tests the following tracing methods and asserts that
            the trace was downloaded successfully.

        This test case calls the following tracing methods:
                * `tracing.initialize(compression=...)`
                * `tracing.start()`
                * `tracing.stop()`
                * `tracing.terminate_and_download(directory="/tmp/")`
        """
        if compression:
            asserts.skip_if(
                not isinstance(
                    self.dut.tracing, tracing_using_ffx.TracingUsingFfx
                ),
                "Compression is only supported when using the FFX tracing backend.",
            )
        else:
            asserts.skip_if(
                isinstance(self.dut.tracing, tracing_using_ffx.TracingUsingFfx),
                "Uncompressed tracing is not supported when using the FFX tracing backend.",
            )

        # Initialize Tracing Session.
        self.dut.tracing.initialize(compression=compression)

        # Start Tracing.
        await self.dut.tracing.start()

        time.sleep(1)

        # Stop Tracing.
        await self.dut.tracing.stop()

        # Terminate the tracing session.
        with tempfile.NamedTemporaryFile(suffix=".fxt") as trace_fxt:
            res = await self.dut.tracing.terminate_and_download(
                directory=os.path.dirname(trace_fxt.name),
                trace_file=os.path.basename(trace_fxt.name),
            )

            asserts.assert_equal(
                res, trace_fxt.name, msg="trace not downloaded"
            )
            asserts.assert_true(
                os.path.exists(trace_fxt.name), msg="trace failed"
            )

            # Verify that the trace is valid and non-empty using trace_processing model
            model = trace_importing.create_model_from_trace_file_path(
                res, without_json_conversion=True
            )
            asserts.assert_true(
                model.processes,
                f"Trace model (compression={compression}) contains no processes, "
                "trace might be empty or invalid.",
            )

    async def test_tracing_session(self) -> None:
        """This test case tests the `tracing.trace_session()` context manager"""
        async with self.dut.tracing.trace_session():
            asserts.assert_true(
                self.dut.tracing.is_session_initialized(),
                msg="Expected trace session to be initialized inside trace_session()",
            )
            asserts.assert_true(
                self.dut.tracing.is_active(),
                msg="Expected trace session to be active inside trace_session()",
            )
        asserts.assert_false(
            self.dut.tracing.is_session_initialized(),
            msg="Expected trace session to be uninitialized after trace_session()",
        )
        asserts.assert_false(
            self.dut.tracing.is_active(),
            msg="Expected trace session to be inactive after trace_session()",
        )

    async def test_tracing_session_download(self) -> None:
        """This test case tests the `tracing.trace_session()` context manager
        and asserts that the trace was downloaded successfully.
        """
        with tempfile.NamedTemporaryFile(suffix=".fxt") as trace_fxt:
            async with self.dut.tracing.trace_session(
                download=True,
                directory=os.path.dirname(trace_fxt.name),
                trace_file=os.path.basename(trace_fxt.name),
            ):
                asserts.assert_true(
                    self.dut.tracing.is_session_initialized(),
                    msg="Expected trace session to be initialized inside trace_session()",
                )
                asserts.assert_true(
                    self.dut.tracing.is_active(),
                    msg="Expected trace session to be active inside trace_session()",
                )
            asserts.assert_false(
                self.dut.tracing.is_session_initialized(),
                msg="Expected trace session to be uninitialized after trace_session()",
            )
            asserts.assert_false(
                self.dut.tracing.is_active(),
                msg="Expected trace session to be inactive after trace_session()",
            )
            asserts.assert_true(
                os.path.exists(trace_fxt.name), msg="trace failed"
            )

    async def test_multi_tracing_session(self) -> None:
        """This test case tests the multiple traces using trace context manager"""
        async with self.dut.tracing.trace_session():
            asserts.assert_true(
                self.dut.tracing.is_active(),
                msg="Expected trace session to be active inside trace_session()",
            )
            await self.dut.tracing.stop()
            asserts.assert_true(
                self.dut.tracing.is_session_initialized(),
                msg="Expected trace session to remain initialized after stop()",
            )
            asserts.assert_false(
                self.dut.tracing.is_active(),
                msg="Expected trace session to be inactive after stop()",
            )
            time.sleep(1)
            await self.dut.tracing.start()
            asserts.assert_true(
                self.dut.tracing.is_active(),
                msg="Expected trace session to be active after start()",
            )
        asserts.assert_false(
            self.dut.tracing.is_session_initialized(),
            msg="Expected trace session to be uninitialized after trace_session()",
        )
        asserts.assert_false(
            self.dut.tracing.is_active(),
            msg="Expected trace session to be inactive after trace_session()",
        )


if __name__ == "__main__":
    test_runner.main()
