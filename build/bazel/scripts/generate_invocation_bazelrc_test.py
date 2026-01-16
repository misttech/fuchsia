#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit-tests for generate_invocation_bazelrc.py functions."""

import os
import sys
import unittest

# Import regenerator.py as a module.
_SCRIPT_DIR = os.path.dirname(__file__)
sys.path.insert(0, _SCRIPT_DIR)
import generate_invocation_bazelrc as gen


class SiblingBuildsLinkTest(unittest.TestCase):
    def test_link(self):
        link = gen.sibling_builds_link(
            "http://go/fake-test/", "TEST_ID", "feed-face"
        )
        self.assertEqual(link, "http://go/fake-test/?q=TEST_ID:feed-face")


class ParentBuildLinkTest(unittest.TestCase):
    def test_led_link(self):
        link = gen.parent_build_link("infra/led/01234")
        self.assertEqual(
            link, "http://go/lucibuild/infra/led/01234/+/build.proto"
        )

    def test_regular_link(self):
        link = gen.parent_build_link("87654321")
        self.assertEqual(link, "http://go/bbid/87654321")


class MetadataOptionTest(unittest.TestCase):
    def test_key_value(self):
        opt = gen.metadata_option("FOOD", "bbq")
        self.assertEqual(opt, "--build_metadata=FOOD=bbq")


class BuildConfigOptionTest(unittest.TestCase):
    def test_single_option(self):
        rc = gen.build_config_option("feature_x", "--bazel-flag=value")
        self.assertEqual(rc, "build:feature_x --bazel-flag=value")


class MetadataBazelrcTest(unittest.TestCase):
    def test_no_id(self):
        with self.assertRaises(KeyError):
            for line in gen.metadata_bazelrc(dict()):
                pass

    def test_uuid(self):
        rc = set(gen.metadata_bazelrc({"FX_BUILD_UUID": "uuid-6767"}))
        self.assertEqual(
            rc,
            {
                "build:sponge --build_metadata=SIBLING_BUILDS_LINK=http://sponge/invocations/?q=FX_BUILD_UUID:uuid-6767",
                "build:resultstore --build_metadata=SIBLING_BUILDS_LINK=http://go/fxbtx/?q=FX_BUILD_UUID:uuid-6767",
            },
        )

    def test_bbid(self):
        rc = set(gen.metadata_bazelrc({"BUILDBUCKET_ID": "8888"}))
        self.assertEqual(
            rc,
            {
                "build:_bes_common --build_metadata=PARENT_BUILD_LINK=http://go/bbid/8888",
                "build:sponge_infra --build_metadata=SIBLING_BUILDS_LINK=http://sponge/invocations/?q=BUILDBUCKET_ID:8888",
                "build:resultstore_infra --build_metadata=SIBLING_BUILDS_LINK=http://go/fxbtx/?q=BUILDBUCKET_ID:8888",
            },
        )


if __name__ == "__main__":
    unittest.main()
