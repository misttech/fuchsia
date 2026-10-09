#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit-tests for generate_invocation_bazelrc.py functions."""

import os
import sys
import tempfile
import unittest

# Import regenerator.py as a module.
_SCRIPT_DIR = os.path.dirname(__file__)
sys.path.insert(0, _SCRIPT_DIR)
import generate_invocation_bazelrc as gen


class BBIDLinkTest(unittest.TestCase):
    def test_led_link(self) -> None:
        link = gen.bbid_link("infra/led/01234")
        self.assertEqual(
            link, "http://go/lucibuild/infra/led/01234/+/build.proto"
        )

    def test_regular_link(self) -> None:
        link = gen.bbid_link("87654321")
        self.assertEqual(link, "http://go/bbid/87654321")


class MetadataOptionTest(unittest.TestCase):
    def test_key_value(self) -> None:
        opt = gen.metadata_option("FOOD", "bbq")
        self.assertEqual(opt, "--build_metadata=FOOD=bbq")


class BuildConfigOptionTest(unittest.TestCase):
    def test_single_option(self) -> None:
        rc = gen.build_config_option("feature_x", "--bazel-flag=value")
        self.assertEqual(rc, "build:feature_x --bazel-flag=value")


class MetadataBazelrcTest(unittest.TestCase):
    def test_no_id(self) -> None:
        self.assertEqual(list(gen.metadata_bazelrc(dict())), [])

    def test_uuid_top_build(self) -> None:
        uuid = "uuid-6767"
        rc = set(gen.metadata_bazelrc({"FX_BUILD_UUID": uuid}))
        self.assertEqual(
            rc,
            {
                f"build:_bes_common --build_metadata=FX_BUILD_UUID={uuid}",
            },
        )

    def test_uuid_sub_build(self) -> None:
        uuid = "uuid-5678"
        parent_id = "dada"
        parent_link = f"go/to/{parent_id}"
        siblings_link = f"go/search_for/?q=PARENT={parent_id}"
        rc = set(
            gen.metadata_bazelrc(
                {
                    "FX_BUILD_UUID": uuid,
                    "RESULTSTORE_PARENT_BUILD_ID": parent_id,
                    "RESULTSTORE_PARENT_BUILD_LINK": parent_link,
                    "RESULTSTORE_SIBLING_BUILDS_LINK": siblings_link,
                }
            )
        )
        self.assertEqual(
            rc,
            {
                f"build:_bes_common --build_metadata=FX_BUILD_UUID={uuid}",
                f"build:_bes_common --build_metadata=PARENT_BUILD_ID={parent_id}",
                f"build:_bes_common --build_metadata=PARENT_BUILD_LINK={parent_link}",
                f"build:_bes_common --build_metadata=SIBLING_BUILDS_LINK={siblings_link}",
            },
        )

    def test_bbid_top_build(self) -> None:
        bbid = "9988"
        rc = set(gen.metadata_bazelrc({"BUILDBUCKET_ID": bbid}))
        self.assertEqual(
            rc,
            {
                f"build:_bes_common --build_metadata=BUILDBUCKET_ID={bbid}",
                f"build:_bes_common --build_metadata=PARENT_BUILD_ID={bbid}",
                f"build:_bes_common --build_metadata=PARENT_BUILD_LINK=http://go/bbid/{bbid}",
            },
        )

    def test_bbid_sub_build(self) -> None:
        bbid = "8877"
        parent_id = "baba"
        parent_link = f"go/to/{parent_id}"
        siblings_link = f"go/search_for/?q=PARENT={parent_id}"
        rc = set(
            gen.metadata_bazelrc(
                {
                    "BUILDBUCKET_ID": bbid,
                    "RESULTSTORE_PARENT_BUILD_ID": parent_id,
                    "RESULTSTORE_PARENT_BUILD_LINK": parent_link,
                    "RESULTSTORE_SIBLING_BUILDS_LINK": siblings_link,
                }
            )
        )
        self.assertEqual(
            rc,
            {
                f"build:_bes_common --build_metadata=BUILDBUCKET_ID={bbid}",
                f"build:_bes_common --build_metadata=PARENT_BUILD_ID={parent_id}",
                f"build:_bes_common --build_metadata=PARENT_BUILD_LINK={parent_link}",
                f"build:_bes_common --build_metadata=SIBLING_BUILDS_LINK={siblings_link}",
            },
        )


class JiriPrebuiltBazelrcTest(unittest.TestCase):
    def setUp(self) -> None:
        self._temp_dir = tempfile.TemporaryDirectory()
        self.fuchsia_dir = os.path.realpath(self._temp_dir.name)
        self.jiri_root = os.path.join(self.fuchsia_dir, ".jiri_root")
        self.prebuilt_dir = os.path.join(self.fuchsia_dir, "prebuilt")
        os.makedirs(self.jiri_root)
        os.makedirs(self.prebuilt_dir)

    def tearDown(self) -> None:
        self._temp_dir.cleanup()

    def test_sandbox_mounts(self) -> None:
        rc = set(gen.jiri_prebuilt_bazelrc({"FUCHSIA_DIR": self.fuchsia_dir}))
        self.assertEqual(
            rc,
            {
                f"build --sandbox_add_mount_pair={self.jiri_root}",
                f"build --sandbox_writable_path={self.jiri_root}",
                f"build --sandbox_add_mount_pair={self.prebuilt_dir}",
                f"build --sandbox_writable_path={self.prebuilt_dir}",
            },
        )

    def test_symlinked_fuchsia_dir_resolved(self) -> None:
        with tempfile.TemporaryDirectory() as link_parent:
            symlink_dir = os.path.join(link_parent, "fuchsia_link")
            os.symlink(self.fuchsia_dir, symlink_dir)
            rc = set(gen.jiri_prebuilt_bazelrc({"FUCHSIA_DIR": symlink_dir}))
            self.assertEqual(
                rc,
                {
                    f"build --sandbox_add_mount_pair={self.jiri_root}",
                    f"build --sandbox_writable_path={self.jiri_root}",
                    f"build --sandbox_add_mount_pair={self.prebuilt_dir}",
                    f"build --sandbox_writable_path={self.prebuilt_dir}",
                },
            )


if __name__ == "__main__":
    unittest.main()
