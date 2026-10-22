# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for Starnix affordance."""

import json
import pathlib
import shutil
from typing import Any

import fuchsia_base_test
import fuchsia_inspect
from honeydew import errors
from honeydew.affordances.starnix import starnix_using_ffx
from mobly import asserts, test_runner


class StarnixAffordanceTests(fuchsia_base_test.FuchsiaBaseTest):
    """Starnix affordance tests"""

    def _assert_one_kernel(
        self, kernels: list[fuchsia_inspect.InspectData]
    ) -> None:
        asserts.assert_equal(len(kernels), 1, f"{kernels}")
        asserts.assert_is_instance(kernels[0], fuchsia_inspect.InspectData)
        asserts.assert_true(
            kernels[0].moniker.startswith(
                starnix_using_ffx._STARNIX_KERNEL_PREFIX
            ),
            f"Unexpected moniker {kernels[0].moniker}",
        )
        asserts.assert_is_not_none(kernels[0].payload)

    def _assert_one_kernel_dict(self, kernels: list[dict[str, Any]]) -> None:
        asserts.assert_equal(len(kernels), 1, f"{kernels}")
        asserts.assert_true(
            kernels[0]["moniker"].startswith(
                starnix_using_ffx._STARNIX_KERNEL_PREFIX
            ),
            f"Unexpected moniker {kernels[0]['moniker']}",
        )
        asserts.assert_is_not_none(kernels[0]["payload"])

    async def test_run_console_shell_cmd(self) -> None:
        """Test case for Starnix.run_console_shell_cmd()"""
        if self.user_params["is_starnix_supported"]:
            await self.dut.starnix.run_console_shell_cmd(["echo", "hello"])
        else:
            with asserts.assert_raises(errors.NotSupportedError):
                await self.dut.starnix.run_console_shell_cmd(["echo", "hello"])

    def test_get_kernel_inspect(self) -> None:
        """Test case for Starnix.get_kernel_inspect()."""
        if self.user_params["is_starnix_supported"]:
            self._assert_one_kernel(self.dut.starnix.get_kernel_inspect())
        else:
            with asserts.assert_raises(errors.NotSupportedError):
                self.dut.starnix.get_kernel_inspect()

    def test_save_kernel_inspect(self) -> None:
        """Test case for Starnix.save_kernel_inspect()."""
        out_dir = pathlib.Path(self.test_case_path) / "inspect"
        if self.user_params["is_starnix_supported"]:
            path = self.dut.starnix.save_kernel_inspect(
                directory=out_dir,
                file="kernel_inspect.json",
            )
            asserts.assert_equal(
                path,
                str((out_dir / "kernel_inspect.json").absolute()),
            )
            self._assert_one_kernel_dict(
                json.loads(pathlib.Path(path).read_text())
            )

            default_path = pathlib.Path(self.dut.starnix.save_kernel_inspect())
            try:
                asserts.assert_true(
                    default_path.exists(), f"{default_path} does not exist"
                )
                self._assert_one_kernel_dict(
                    json.loads(default_path.read_text())
                )
            finally:
                shutil.rmtree(default_path.parent, ignore_errors=True)
        else:
            with asserts.assert_raises(errors.NotSupportedError):
                self.dut.starnix.save_kernel_inspect(directory=out_dir)


if __name__ == "__main__":
    test_runner.main()
