#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit-tests for generate_invocation_bazelrc.py functions."""

import os
import tempfile
import unittest

from build.bazel.scripts import bazel_invocation as bazel_inv
from build.bazel.scripts import generate_invocation_bazelrc as gen
from build.resultstore import related_invocations as rel_inv

_TEST_RESULTS_URL = "http://go/test-rs"


def _bes_meta(key: str, value: str) -> str:
    return f"build:_bes_common --build_metadata={key}={value}"


class MetadataOptionTest(unittest.TestCase):
    def test_key_value(self) -> None:
        opt = gen._metadata_option("FOOD", "bbq")
        self.assertEqual(opt, "--build_metadata=FOOD=bbq")


class BuildConfigOptionTest(unittest.TestCase):
    def test_single_option(self) -> None:
        rc = gen.build_config_option("feature_x", "--bazel-flag=value")
        self.assertEqual(rc, "build:feature_x --bazel-flag=value")


class MetadataBazelrcTest(unittest.TestCase):
    def test_no_id(self) -> None:
        self.assertEqual(list(gen._metadata_bazelrc({})), [])

    def test_uuid_top_build(self) -> None:
        uuid = "uuid-6767"
        env = {
            rel_inv.ENV_FX_BUILD_UUID: uuid,
            rel_inv.ENV_RESULTS_URL: _TEST_RESULTS_URL,
        }
        self.assertEqual(
            set(gen._metadata_bazelrc(env)),
            {
                _bes_meta(rel_inv.KEY_FX_BUILD_UUID, uuid),
                _bes_meta(
                    rel_inv.KEY_SIBLING_BUILDS_LINK,
                    f"{_TEST_RESULTS_URL}/?q={rel_inv.KEY_FX_BUILD_UUID}:{uuid}",
                ),
                _bes_meta(
                    rel_inv.KEY_SUB_BUILDS_LINK,
                    f"{_TEST_RESULTS_URL}/?q={rel_inv.KEY_PARENT_BUILD_ID}:{uuid}",
                ),
            },
        )

    def test_uuid_sub_build(self) -> None:
        uuid = "fake-uuid-5678"
        parent_id = "fake-parent-id"
        parent_link = f"go/to/{parent_id}"
        siblings_link = f"go/search_for/?q=PARENT={parent_id}"
        env = {
            rel_inv.ENV_FX_BUILD_UUID: uuid,
            rel_inv.ENV_PARENT_BUILD_ID: parent_id,
            rel_inv.ENV_PARENT_BUILD_LINK: parent_link,
            rel_inv.ENV_SIBLING_BUILDS_LINK: siblings_link,
            rel_inv.ENV_RESULTS_URL: _TEST_RESULTS_URL,
        }
        self.assertTrue(
            {
                _bes_meta(rel_inv.KEY_FX_BUILD_UUID, uuid),
                _bes_meta(rel_inv.KEY_PARENT_BUILD_ID, parent_id),
                _bes_meta(rel_inv.KEY_PARENT_BUILD_LINK, parent_link),
                _bes_meta(rel_inv.KEY_SIBLING_BUILDS_LINK, siblings_link),
            }.issubset(set(gen._metadata_bazelrc(env)))
        )

    def test_bbid_top_build(self) -> None:
        bbid = "fake-bbid-9988"
        env = {
            rel_inv.ENV_BUILDBUCKET_ID: bbid,
            rel_inv.ENV_RESULTS_URL: _TEST_RESULTS_URL,
        }
        self.assertTrue(
            {
                _bes_meta(rel_inv.KEY_BUILDBUCKET_ID, bbid),
                _bes_meta(rel_inv.KEY_PARENT_BUILD_ID, bbid),
                _bes_meta(
                    rel_inv.KEY_PARENT_BUILD_LINK, f"http://go/bbid/{bbid}"
                ),
                _bes_meta(
                    rel_inv.KEY_SIBLING_BUILDS_LINK,
                    f"{_TEST_RESULTS_URL}/?q={rel_inv.KEY_BUILDBUCKET_ID}:{bbid}",
                ),
            }.issubset(set(gen._metadata_bazelrc(env)))
        )

    def test_bbid_sub_build(self) -> None:
        bbid = "fake-bbid-8877"
        parent_id = "fake-parent-id"
        parent_link = f"go/to/{parent_id}"
        siblings_link = f"go/search_for/?q=PARENT={parent_id}"
        env = {
            rel_inv.ENV_BUILDBUCKET_ID: bbid,
            rel_inv.ENV_PARENT_BUILD_ID: parent_id,
            rel_inv.ENV_PARENT_BUILD_LINK: parent_link,
            rel_inv.ENV_SIBLING_BUILDS_LINK: siblings_link,
            rel_inv.ENV_RESULTS_URL: _TEST_RESULTS_URL,
        }
        self.assertTrue(
            {
                _bes_meta(rel_inv.KEY_BUILDBUCKET_ID, bbid),
                _bes_meta(rel_inv.KEY_PARENT_BUILD_ID, parent_id),
                _bes_meta(rel_inv.KEY_PARENT_BUILD_LINK, parent_link),
                _bes_meta(rel_inv.KEY_SIBLING_BUILDS_LINK, siblings_link),
            }.issubset(set(gen._metadata_bazelrc(env)))
        )


class JiriPrebuiltBazelrcTest(unittest.TestCase):
    def setUp(self) -> None:
        self._temp_dir = tempfile.TemporaryDirectory()
        self.fuchsia_dir = os.path.realpath(self._temp_dir.name)
        self.jiri_root = os.path.join(self.fuchsia_dir, ".jiri_root")
        self.prebuilt_dir = os.path.join(self.fuchsia_dir, "prebuilt")
        os.makedirs(self.jiri_root)
        os.makedirs(self.prebuilt_dir)
        self.expected_mounts = {
            f"build --sandbox_add_mount_pair={self.jiri_root}",
            f"build --sandbox_writable_path={self.jiri_root}",
            f"build --sandbox_add_mount_pair={self.prebuilt_dir}",
            f"build --sandbox_writable_path={self.prebuilt_dir}",
        }

    def tearDown(self) -> None:
        self._temp_dir.cleanup()

    def test_sandbox_mounts(self) -> None:
        rc = set(gen._jiri_prebuilt_bazelrc({"FUCHSIA_DIR": self.fuchsia_dir}))
        self.assertEqual(rc, self.expected_mounts)

    def test_symlinked_fuchsia_dir_resolved(self) -> None:
        with tempfile.TemporaryDirectory() as link_parent:
            symlink_dir = os.path.join(link_parent, "fuchsia_link")
            os.symlink(self.fuchsia_dir, symlink_dir)
            rc = set(gen._jiri_prebuilt_bazelrc({"FUCHSIA_DIR": symlink_dir}))
            self.assertEqual(rc, self.expected_mounts)


class AuthBazelrcTest(unittest.TestCase):
    def test_loas_auth(self) -> None:
        self.assertEqual(
            list(gen._auth_bazelrc({"FX_BUILD_RBE_AUTH_TYPE": "loas"})),
            [f"build:_remote_common --config={bazel_inv.CONFIG_GCERTAUTH}"],
        )

    def test_non_loas_auth(self) -> None:
        self.assertEqual(
            list(gen._auth_bazelrc({"FX_BUILD_RBE_AUTH_TYPE": "oauth"})), []
        )
        self.assertEqual(list(gen._auth_bazelrc({})), [])


class ServiceProxiesBazelrcTest(unittest.TestCase):
    def test_proxy_sockets(self) -> None:
        env = {
            bazel_inv.ENV_BAZEL_RBE_SOCKET: "/tmp/fake-rbe.sock",
            bazel_inv.ENV_BAZEL_RESULTSTORE_SOCKET: "/tmp/fake-rs.sock",
        }
        rc = list(gen._service_proxies_bazelrc(env))
        self.assertIn(
            "build:_remote_common --remote_proxy=unix:///tmp/fake-rbe.sock", rc
        )
        self.assertIn(
            "build:_resultstore --bes_proxy=unix:///tmp/fake-rs.sock", rc
        )


class GenerateBazelrcTest(unittest.TestCase):
    def test_generate_bazelrc_with_metadata(self) -> None:
        env = {
            rel_inv.ENV_FX_BUILD_UUID: "fake-uuid-1",
            rel_inv.ENV_RESULTS_URL: "http://go/fake-rs",
        }
        rc = list(gen.generate_bazelrc(env))
        self.assertIn(_bes_meta(rel_inv.KEY_FX_BUILD_UUID, "fake-uuid-1"), rc)
        self.assertIn(
            _bes_meta(
                rel_inv.KEY_SIBLING_BUILDS_LINK,
                "http://go/fake-rs/?q=FX_BUILD_UUID:fake-uuid-1",
            ),
            rc,
        )
        self.assertIn(
            _bes_meta(
                rel_inv.KEY_SUB_BUILDS_LINK,
                "http://go/fake-rs/?q=PARENT_BUILD_ID:fake-uuid-1",
            ),
            rc,
        )


if __name__ == "__main__":
    unittest.main()
