#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for rsninja.py."""

import unittest
from pathlib import Path
from unittest import mock

from build.resultstore import related_invocations as rel_inv
from build.resultstore import rs_profile, rsninja


class PrepareNinjaInvocationTest(unittest.TestCase):
    def test_top_level_fx_build(self) -> None:
        environ = {
            "FX_BUILD_UUID": "fake-fx-uuid-1",
            "RS_results_url": "http://go/fake-rs",
        }
        cmd, child_env = rsninja.prepare_ninja_invocation(
            wrapper="/path/to/rs-ninja.sh",
            ninja_bin="/path/to/ninja",
            ninja_args=["-C", "out/default", "target"],
            environ=environ,
        )

        self.assertEqual(
            cmd,
            [
                "/path/to/rs-ninja.sh",
                "/path/to/ninja",
                "--bes_metadata=FX_BUILD_UUID=fake-fx-uuid-1",
                "--bes_metadata=SIBLING_BUILDS_LINK=http://go/fake-rs/?q=FX_BUILD_UUID:fake-fx-uuid-1",
                "--bes_metadata=SUB_BUILDS_LINK=http://go/fake-rs/?q=PARENT_BUILD_ID:fake-fx-uuid-1",
                "-C",
                "out/default",
                "target",
            ],
        )

        self.assertEqual(child_env["NINJA_BUILD_ID"], "fake-fx-uuid-1")
        self.assertEqual(
            child_env[rel_inv.ENV_PARENT_BUILD_ID], "fake-fx-uuid-1"
        )
        self.assertEqual(
            child_env[rel_inv.ENV_SIBLING_BUILDS_LINK],
            "http://go/fake-rs/?q=PARENT_BUILD_ID:fake-fx-uuid-1",
        )

    def test_sub_build(self) -> None:
        environ = {
            "RESULTSTORE_PARENT_BUILD_ID": "fake-parent-id",
            "RESULTSTORE_PARENT_BUILD_LINK": "http://go/fake-rs/fake-parent-id",
            "RESULTSTORE_SIBLING_BUILDS_LINK": "http://go/fake-rs/?q=PARENT_BUILD_ID:fake-parent-id",
            "RS_results_url": "http://go/fake-rs",
        }
        cmd, child_env = rsninja.prepare_ninja_invocation(
            wrapper="/path/to/rs-ninja.sh",
            ninja_bin="/path/to/ninja",
            ninja_args=["target"],
            environ=environ,
        )

        sub_build_id = child_env["NINJA_BUILD_ID"]
        self.assertNotEqual(sub_build_id, "fake-parent-id")
        self.assertEqual(
            cmd,
            [
                "/path/to/rs-ninja.sh",
                "/path/to/ninja",
                "--bes_metadata=PARENT_BUILD_ID=fake-parent-id",
                "--bes_metadata=PARENT_BUILD_LINK=http://go/fake-rs/fake-parent-id",
                "--bes_metadata=SIBLING_BUILDS_LINK=http://go/fake-rs/?q=PARENT_BUILD_ID:fake-parent-id",
                f"--bes_metadata=SUB_BUILDS_LINK=http://go/fake-rs/?q=PARENT_BUILD_ID:{sub_build_id}",
                "target",
            ],
        )

        # Sub-build gets its own new UUID for NINJA_BUILD_ID
        self.assertEqual(
            child_env[rel_inv.ENV_PARENT_BUILD_LINK],
            f"http://go/fake-rs/{sub_build_id}",
        )

    @mock.patch.object(rs_profile, "load")
    def test_fallback_to_rs_profile_when_results_url_missing(
        self, mock_load: mock.MagicMock
    ) -> None:
        mock_load.return_value = mock.MagicMock(
            results_url="http://go/profile-rs"
        )
        environ = {
            "FX_BUILD_UUID": "fake-fx-uuid-2",
        }
        cmd, child_env = rsninja.prepare_ninja_invocation(
            wrapper="/path/to/rs-ninja.sh",
            ninja_bin="/path/to/ninja",
            ninja_args=["target"],
            environ=environ,
            fuchsia_dir=Path("/fake/fuchsia"),
        )
        self.assertEqual(
            child_env[rel_inv.ENV_SIBLING_BUILDS_LINK],
            "http://go/profile-rs/?q=PARENT_BUILD_ID:fake-fx-uuid-2",
        )

    @mock.patch.object(rs_profile, "load")
    def test_missing_results_url_with_rsproxy_fifo_raises(
        self, mock_load: mock.MagicMock
    ) -> None:
        mock_load.side_effect = Exception("profile load failed")
        environ = {
            "RSPROXY_FIFO": "/tmp/fake-fifo",
            "FX_BUILD_UUID": "fake-fx-uuid-2",
        }
        with self.assertRaises(RuntimeError) as cm:
            rsninja.prepare_ninja_invocation(
                wrapper="/path/to/rs-ninja.sh",
                ninja_bin="/path/to/ninja",
                ninja_args=["target"],
                environ=environ,
                fuchsia_dir=Path("/fake/fuchsia"),
            )
        self.assertIn("Unable to determine ResultStore URL", str(cm.exception))


if __name__ == "__main__":
    unittest.main()
