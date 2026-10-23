#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for bazel_invocation.py."""

import unittest
from pathlib import Path

from build.bazel.scripts import bazel_invocation as bi


class SocketEnvVarsTest(unittest.TestCase):
    def test_empty_sockets(self) -> None:
        self.assertEqual(bi.socket_env_vars(), {})

    def test_rbe_socket_only(self) -> None:
        env = bi.socket_env_vars(rbe_socket=Path("/tmp/fake-rbe.sock"))
        self.assertEqual(
            env,
            {
                bi.ENV_BAZEL_RBE_SOCKET: "/tmp/fake-rbe.sock",
            },
        )

    def test_resultstore_socket_only(self) -> None:
        env = bi.socket_env_vars(resultstore_socket="/tmp/fake-rs.sock")
        self.assertEqual(
            env,
            {
                bi.ENV_BAZEL_RESULTSTORE_SOCKET: "/tmp/fake-rs.sock",
            },
        )

    def test_both_sockets(self) -> None:
        env = bi.socket_env_vars(
            rbe_socket="/tmp/fake-rbe.sock",
            resultstore_socket="/tmp/fake-rs.sock",
        )
        self.assertEqual(
            env,
            {
                bi.ENV_BAZEL_RBE_SOCKET: "/tmp/fake-rbe.sock",
                bi.ENV_BAZEL_RESULTSTORE_SOCKET: "/tmp/fake-rs.sock",
            },
        )


class ResultStoreBazelConfigTest(unittest.TestCase):
    def test_disabled_mode(self) -> None:
        self.assertIsNone(
            bi.resultstore_bazel_config("none", "loas", in_infra=False)
        )
        self.assertIsNone(
            bi.resultstore_bazel_config("ninja", "loas", in_infra=False)
        )

    def test_developer_loas(self) -> None:
        self.assertEqual(
            bi.resultstore_bazel_config("bazel", "loas", in_infra=False),
            bi.CONFIG_RESULTSTORE,
        )
        self.assertEqual(
            bi.resultstore_bazel_config("all", "loas", in_infra=False),
            bi.CONFIG_RESULTSTORE,
        )

    def test_developer_oauth(self) -> None:
        self.assertEqual(
            bi.resultstore_bazel_config("bazel", "oauth", in_infra=False),
            bi.CONFIG_RESULTSTORE_INFRA,
        )

    def test_infra_loas(self) -> None:
        self.assertEqual(
            bi.resultstore_bazel_config("all", "loas", in_infra=True),
            bi.CONFIG_RESULTSTORE_INFRA,
        )
        self.assertEqual(
            bi.resultstore_bazel_config("bazel", "loas", in_infra=True),
            bi.CONFIG_RESULTSTORE_INFRA,
        )


class EnabledBazelResultStoreTest(unittest.TestCase):
    def test_modes(self) -> None:
        self.assertTrue(bi._enabled_bazel_resultstore("all"))
        self.assertTrue(bi._enabled_bazel_resultstore("bazel"))
        self.assertFalse(bi._enabled_bazel_resultstore("ninja"))
        self.assertFalse(bi._enabled_bazel_resultstore("none"))
        self.assertFalse(bi._enabled_bazel_resultstore(""))


class NeedsGcertauthTest(unittest.TestCase):
    def test_loas_rbe(self) -> None:
        self.assertTrue(bi._needs_gcertauth("loas", "none"))

    def test_loas_resultstore(self) -> None:
        self.assertTrue(bi._needs_gcertauth("none", "loas"))

    def test_loas_both(self) -> None:
        self.assertTrue(bi._needs_gcertauth("loas", "loas"))

    def test_neither_loas(self) -> None:
        self.assertFalse(bi._needs_gcertauth("oauth", "oauth"))
        self.assertFalse(bi._needs_gcertauth("machine", "machine"))
        self.assertFalse(bi._needs_gcertauth("none", "none"))


class GenerateBazelrcTest(unittest.TestCase):
    def test_service_proxies_empty(self) -> None:
        self.assertEqual(list(bi.generate_service_proxies_bazelrc({})), [])

    def test_service_proxies_rbe(self) -> None:
        lines = list(
            bi.generate_service_proxies_bazelrc(
                {bi.ENV_BAZEL_RBE_SOCKET: "/tmp/fake-rbe.sock"}
            )
        )
        self.assertEqual(
            lines,
            [
                f"build:{bi.CONFIG_REMOTE_COMMON} --remote_proxy=unix:///tmp/fake-rbe.sock"
            ],
        )

    def test_service_proxies_resultstore(self) -> None:
        lines = list(
            bi.generate_service_proxies_bazelrc(
                {bi.ENV_BAZEL_RESULTSTORE_SOCKET: "/tmp/fake-rs.sock"}
            )
        )
        self.assertEqual(
            lines,
            [
                f"build:{bi.CONFIG_RESULTSTORE_COMMON} --bes_proxy=unix:///tmp/fake-rs.sock"
            ],
        )

    def test_generate_auth_bazelrc_loas(self) -> None:
        lines = list(bi.generate_auth_bazelrc("loas", "none"))
        self.assertEqual(
            lines,
            [f"build:{bi.CONFIG_REMOTE_COMMON} --config={bi.CONFIG_GCERTAUTH}"],
        )

    def test_generate_auth_bazelrc_non_loas(self) -> None:
        self.assertEqual(list(bi.generate_auth_bazelrc("oauth", "none")), [])
        self.assertEqual(list(bi.generate_auth_bazelrc("none", "none")), [])


if __name__ == "__main__":
    unittest.main()
