#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit-tests for generate_invocation_bazelrc.py functions."""

import contextlib
import io
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from build.bazel.scripts import bazel_invocation as bazel_inv
from build.bazel.scripts import generate_invocation_bazelrc as gen
from build.rbe import casfs
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
        config = gen.generate_invocation({})
        self.assertFalse(
            any("--build_metadata=" in line for line in config.bazelrc_lines)
        )
        self.assertEqual(
            config.child_env[rel_inv.ENV_PARENT_BUILD_ID], config.invocation_id
        )

    def test_uuid_top_build(self) -> None:
        uuid = "uuid-6767"
        env = {
            rel_inv.ENV_FX_BUILD_UUID: uuid,
            rel_inv.ENV_RESULTS_URL: _TEST_RESULTS_URL,
        }
        config = gen.generate_invocation(env)
        self.assertEqual(config.invocation_id, uuid)
        self.assertEqual(config.child_env[rel_inv.ENV_PARENT_BUILD_ID], uuid)
        self.assertEqual(
            config.child_env[rel_inv.ENV_PARENT_BUILD_LINK],
            f"{_TEST_RESULTS_URL}/{uuid}",
        )
        rc = set(config.bazelrc_lines)
        self.assertTrue(
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
            }.issubset(rc)
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
        config = gen.generate_invocation(env)
        self.assertNotEqual(config.invocation_id, parent_id)
        self.assertEqual(
            config.child_env[rel_inv.ENV_PARENT_BUILD_ID], config.invocation_id
        )
        rc = set(config.bazelrc_lines)
        self.assertTrue(
            {
                _bes_meta(rel_inv.KEY_FX_BUILD_UUID, uuid),
                _bes_meta(rel_inv.KEY_PARENT_BUILD_ID, parent_id),
                _bes_meta(rel_inv.KEY_PARENT_BUILD_LINK, parent_link),
                _bes_meta(rel_inv.KEY_SIBLING_BUILDS_LINK, siblings_link),
            }.issubset(rc)
        )

    def test_bbid_top_build(self) -> None:
        bbid = "fake-bbid-9988"
        env = {
            rel_inv.ENV_BUILDBUCKET_ID: bbid,
            rel_inv.ENV_RESULTS_URL: _TEST_RESULTS_URL,
        }
        config = gen.generate_invocation(env)
        self.assertEqual(
            config.child_env[rel_inv.ENV_PARENT_BUILD_ID], config.invocation_id
        )
        self.assertEqual(
            config.child_env[rel_inv.ENV_PARENT_BUILD_LINK],
            f"{_TEST_RESULTS_URL}/{config.invocation_id}",
        )
        rc = set(config.bazelrc_lines)
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
            }.issubset(rc)
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
        config = gen.generate_invocation(env)
        self.assertEqual(
            config.child_env[rel_inv.ENV_PARENT_BUILD_ID], config.invocation_id
        )
        rc = set(config.bazelrc_lines)
        self.assertTrue(
            {
                _bes_meta(rel_inv.KEY_BUILDBUCKET_ID, bbid),
                _bes_meta(rel_inv.KEY_PARENT_BUILD_ID, parent_id),
                _bes_meta(rel_inv.KEY_PARENT_BUILD_LINK, parent_link),
                _bes_meta(rel_inv.KEY_SIBLING_BUILDS_LINK, siblings_link),
            }.issubset(rc)
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


class ResultStoreConfigBazelrcTest(unittest.TestCase):
    def test_resultstore_config(self) -> None:
        rc = list(
            gen._resultstore_config_bazelrc(
                {bazel_inv.ENV_RESULTSTORE_BAZEL: bazel_inv.CONFIG_RESULTSTORE}
            )
        )
        self.assertEqual(rc, [f"build --config={bazel_inv.CONFIG_RESULTSTORE}"])

    def test_resultstore_infra_config(self) -> None:
        rc = list(
            gen._resultstore_config_bazelrc(
                {
                    bazel_inv.ENV_RESULTSTORE_BAZEL: bazel_inv.CONFIG_RESULTSTORE_INFRA
                }
            )
        )
        self.assertEqual(
            rc, [f"build --config={bazel_inv.CONFIG_RESULTSTORE_INFRA}"]
        )

    def test_disabled_config(self) -> None:
        self.assertEqual(
            list(
                gen._resultstore_config_bazelrc(
                    {bazel_inv.ENV_RESULTSTORE_BAZEL: "0"}
                )
            ),
            [],
        )
        self.assertEqual(list(gen._resultstore_config_bazelrc({})), [])

    def test_invalid_config_raises(self) -> None:
        with self.assertRaises(ValueError) as ctx:
            list(
                gen._resultstore_config_bazelrc(
                    {bazel_inv.ENV_RESULTSTORE_BAZEL: "invalid_mode"}
                )
            )
        self.assertIn(
            "Invalid FX_INTERNAL_RESULTSTORE_BAZEL", str(ctx.exception)
        )


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

    def test_rbe_casfs_virtualize_enabled(self) -> None:
        rc = set(
            gen._service_proxies_bazelrc(
                {
                    casfs.ENV_CASFS_VIRTUALIZE: "1",
                }
            )
        )
        self.assertIn(
            f"startup --unix_digest_hash_attribute_name={casfs.XATTR_RBE_ARI_BIN_HASH}",
            rc,
        )

    def test_rbe_casfs_virtualize_disabled(self) -> None:
        rc = set(
            gen._service_proxies_bazelrc(
                {
                    casfs.ENV_CASFS_VIRTUALIZE: "0",
                }
            )
        )
        self.assertNotIn(
            f"startup --unix_digest_hash_attribute_name={casfs.XATTR_RBE_ARI_BIN_HASH}",
            rc,
        )


class GenerateInvocationTest(unittest.TestCase):
    def test_generate_invocation_with_metadata(self) -> None:
        env = {
            rel_inv.ENV_FX_BUILD_UUID: "fake-uuid-1",
            rel_inv.ENV_RESULTS_URL: "http://go/fake-rs",
        }
        config = gen.generate_invocation(env)
        rc = config.bazelrc_lines
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

    def test_explicit_invocation_id(self) -> None:
        env = {
            rel_inv.ENV_FX_BUILD_UUID: "parent-uuid",
            rel_inv.ENV_PARENT_BUILD_ID: "parent-uuid",
            rel_inv.ENV_RESULTS_URL: "http://go/fake-rs",
        }
        config = gen.generate_invocation(env, invocation_id="custom-child-id")
        self.assertEqual(config.invocation_id, "custom-child-id")
        self.assertIn(
            "build --invocation_id=custom-child-id", config.bazelrc_lines
        )
        self.assertEqual(
            config.child_env[rel_inv.ENV_PARENT_BUILD_ID], "custom-child-id"
        )

    def test_log_dir_options(self) -> None:
        log_dir = Path("/tmp/fake/log_dir")
        config = gen.generate_invocation({}, log_dir=log_dir)
        self.assertIn(
            "common --experimental_workspace_rules_log_file=/tmp/fake/log_dir/workspace_events.log",
            config.bazelrc_lines,
        )
        self.assertIn(
            "common:exec_log --execution_log_compact_file=/tmp/fake/log_dir/exec_log.pb.zstd",
            config.bazelrc_lines,
        )
        self.assertIn(
            "common:_profiling --profile=/tmp/fake/log_dir/command.profile.gz",
            config.bazelrc_lines,
        )


class MainEnvWrapperTest(unittest.TestCase):
    @mock.patch.object(os, "execvpe")
    def test_main_exec_mode(self, mock_exec: mock.MagicMock) -> None:
        with tempfile.TemporaryDirectory() as td:
            rc_path = Path(td) / "test.bazelrc"
            gen.main(
                [
                    f"--output-bazelrc={rc_path}",
                    "--invocation-id=test-id-123",
                    "--",
                    "echo",
                    "hello",
                ]
            )
            self.assertTrue(rc_path.is_file())
            content = rc_path.read_text()
            self.assertIn("build --invocation_id=test-id-123", content)
            mock_exec.assert_called_once()
            cmd, child_env = (
                mock_exec.call_args[0][1],
                mock_exec.call_args[0][2],
            )
            self.assertEqual(cmd, ["echo", "hello"])
            self.assertEqual(
                child_env[rel_inv.ENV_PARENT_BUILD_ID], "test-id-123"
            )

    @mock.patch.object(os, "execvpe")
    def test_main_stdout_with_log_dir_saves_copy(
        self, mock_exec: mock.MagicMock
    ) -> None:
        with tempfile.TemporaryDirectory() as td:
            log_dir = Path(td) / "bazel_logs"
            log_dir.mkdir()
            out_buf = io.StringIO()
            with contextlib.redirect_stdout(out_buf):
                gen.main([f"--log-dir={log_dir}"])
            stdout_content = out_buf.getvalue()
            saved_copy = log_dir / "invocation.bazelrc"
            self.assertTrue(saved_copy.is_file())
            disk_content = saved_copy.read_text()
            self.assertIn("build --invocation_id=", stdout_content)
            self.assertEqual(stdout_content, disk_content)

    def test_main_wrap_without_output_bazelrc_errors(self) -> None:
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                gen.main(["--", "echo", "hello"])

    @mock.patch.object(os, "execvpe")
    def test_main_empty_log_dir_ignored(
        self, mock_exec: mock.MagicMock
    ) -> None:
        out_buf = io.StringIO()
        with contextlib.redirect_stdout(out_buf):
            gen.main(["--log-dir="])
        self.assertIn("build --invocation_id=", out_buf.getvalue())

    @mock.patch.object(os, "execvpe")
    @mock.patch.object(Path, "write_text")
    def test_main_log_dir_oserror_prints_warning(
        self, mock_write: mock.MagicMock, mock_exec: mock.MagicMock
    ) -> None:
        mock_write.side_effect = OSError("Read-only file system")
        err_buf = io.StringIO()
        out_buf = io.StringIO()
        with (
            contextlib.redirect_stderr(err_buf),
            contextlib.redirect_stdout(out_buf),
        ):
            gen.main(["--log-dir=/fake/read_only_dir"])
        self.assertIn("WARNING: Failed to save debug copy", err_buf.getvalue())
        self.assertIn("Read-only file system", err_buf.getvalue())
        self.assertIn("build --invocation_id=", out_buf.getvalue())


if __name__ == "__main__":
    unittest.main()
