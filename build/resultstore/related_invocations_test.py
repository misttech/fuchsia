#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for related_invocations.py."""

import unittest

from build.resultstore import related_invocations as rel

_FAKE_RESULTS_URL = "http://go/fake-rs"


class ResolveRelatedInvocationsTest(unittest.TestCase):
    def test_resolve_empty_environ(self) -> None:
        linkage = rel.resolve(
            {},
            results_url=_FAKE_RESULTS_URL,
            invocation_id="fake-invocation-id-1",
        )
        self.assertEqual(linkage.invocation_id, "fake-invocation-id-1")
        self.assertEqual(
            linkage.metadata,
            {
                rel.KEY_SUB_BUILDS_LINK: "http://go/fake-rs/?q=PARENT_BUILD_ID:fake-invocation-id-1",
            },
        )
        self.assertEqual(
            linkage.child_env,
            {
                rel.ENV_PARENT_BUILD_ID: "fake-invocation-id-1",
                rel.ENV_RESULTS_URL: _FAKE_RESULTS_URL,
                rel.ENV_PARENT_BUILD_LINK: "http://go/fake-rs/fake-invocation-id-1",
                rel.ENV_SIBLING_BUILDS_LINK: "http://go/fake-rs/?q=PARENT_BUILD_ID:fake-invocation-id-1",
            },
        )

    def test_resolve_top_level_fx_build_default_id(self) -> None:
        env = {
            rel.ENV_FX_BUILD_UUID: "fake-fx-uuid-1",
            rel.ENV_RESULTS_URL: _FAKE_RESULTS_URL,
        }
        linkage = rel.resolve(env)
        # Automatically resolves invocation_id to FX_BUILD_UUID for top-level fx builds
        self.assertEqual(linkage.invocation_id, "fake-fx-uuid-1")
        self.assertEqual(
            linkage.metadata,
            {
                rel.KEY_FX_BUILD_UUID: "fake-fx-uuid-1",
                rel.KEY_SIBLING_BUILDS_LINK: "http://go/fake-rs/?q=FX_BUILD_UUID:fake-fx-uuid-1",
                rel.KEY_SUB_BUILDS_LINK: "http://go/fake-rs/?q=PARENT_BUILD_ID:fake-fx-uuid-1",
            },
        )
        self.assertEqual(
            linkage.child_env[rel.ENV_PARENT_BUILD_ID], "fake-fx-uuid-1"
        )
        self.assertEqual(
            linkage.child_env[rel.ENV_PARENT_BUILD_LINK],
            "http://go/fake-rs/fake-fx-uuid-1",
        )

    def test_resolve_top_level_fx_build_explicit_id(self) -> None:
        env = {
            rel.ENV_FX_BUILD_UUID: "fake-fx-uuid-1",
            rel.ENV_RESULTS_URL: _FAKE_RESULTS_URL,
        }
        linkage = rel.resolve(env, invocation_id="explicit-id-2")
        self.assertEqual(linkage.invocation_id, "explicit-id-2")
        self.assertEqual(
            linkage.metadata,
            {
                rel.KEY_FX_BUILD_UUID: "fake-fx-uuid-1",
                rel.KEY_SIBLING_BUILDS_LINK: "http://go/fake-rs/?q=FX_BUILD_UUID:fake-fx-uuid-1",
                rel.KEY_SUB_BUILDS_LINK: "http://go/fake-rs/?q=PARENT_BUILD_ID:explicit-id-2",
            },
        )

    def test_resolve_top_level_infra_build(self) -> None:
        env = {
            rel.ENV_BUILDBUCKET_ID: "fake-bbid-999",
            rel.ENV_BUILDBUCKET_BUILDER: "fake-builder-name",
            rel.ENV_RESULTS_URL: _FAKE_RESULTS_URL,
        }
        linkage = rel.resolve(env, invocation_id="fake-invocation-id-3")
        self.assertEqual(linkage.invocation_id, "fake-invocation-id-3")
        self.assertEqual(
            linkage.metadata,
            {
                rel.KEY_BUILDBUCKET_ID: "fake-bbid-999",
                rel.KEY_BUILDBUCKET_BUILDER: "fake-builder-name",
                rel.KEY_PARENT_BUILD_ID: "fake-bbid-999",
                rel.KEY_PARENT_BUILD_LINK: "http://go/bbid/fake-bbid-999",
                rel.KEY_SIBLING_BUILDS_LINK: "http://go/fake-rs/?q=BUILDBUCKET_ID:fake-bbid-999",
                rel.KEY_SUB_BUILDS_LINK: "http://go/fake-rs/?q=PARENT_BUILD_ID:fake-invocation-id-3",
            },
        )

    def test_resolve_infra_build_with_fx_build_uuid(self) -> None:
        """Verifies that when both BUILDBUCKET_ID and FX_BUILD_UUID are present, BUILDBUCKET_ID is parent_id."""
        env = {
            rel.ENV_FX_BUILD_UUID: "fake-uuid-1",
            rel.ENV_BUILDBUCKET_ID: "fake-bbid-999",
            rel.ENV_BUILDBUCKET_BUILDER: "fake-builder-name",
            rel.ENV_RESULTS_URL: _FAKE_RESULTS_URL,
        }
        linkage = rel.resolve(env, invocation_id="fake-inv-1")
        self.assertEqual(linkage.invocation_id, "fake-inv-1")
        self.assertEqual(
            linkage.metadata[rel.KEY_PARENT_BUILD_ID], "fake-bbid-999"
        )
        self.assertEqual(
            linkage.metadata[rel.KEY_PARENT_BUILD_LINK],
            "http://go/bbid/fake-bbid-999",
        )
        self.assertEqual(linkage.metadata[rel.KEY_FX_BUILD_UUID], "fake-uuid-1")
        self.assertEqual(
            linkage.metadata[rel.KEY_BUILDBUCKET_BUILDER], "fake-builder-name"
        )

    def test_resolve_led_infra_build(self) -> None:
        env = {
            rel.ENV_BUILDBUCKET_ID: "user/led/12345/+/build.proto",
            rel.ENV_RESULTS_URL: _FAKE_RESULTS_URL,
        }
        linkage = rel.resolve(env, invocation_id="fake-invocation-id-4")
        self.assertEqual(
            linkage.metadata[rel.KEY_PARENT_BUILD_LINK],
            "http://go/lucibuild/user/led/12345/+/build.proto",
        )

        # Also test LED ID without the suffix:
        env_no_suffix = {
            rel.ENV_BUILDBUCKET_ID: "user/led/12345",
            rel.ENV_RESULTS_URL: _FAKE_RESULTS_URL,
        }
        linkage_no_suffix = rel.resolve(
            env_no_suffix, invocation_id="fake-invocation-id-5"
        )
        self.assertEqual(
            linkage_no_suffix.metadata[rel.KEY_PARENT_BUILD_LINK],
            "http://go/lucibuild/user/led/12345/+/build.proto",
        )

    def test_resolve_sub_build(self) -> None:
        env = {
            rel.ENV_PARENT_BUILD_ID: "fake-parent-id-888",
            rel.ENV_PARENT_BUILD_LINK: "http://go/fake-rs/fake-parent-id-888",
            rel.ENV_SIBLING_BUILDS_LINK: "http://go/fake-rs/?q=PARENT_BUILD_ID:fake-parent-id-888",
            rel.ENV_RESULTS_URL: _FAKE_RESULTS_URL,
        }
        linkage = rel.resolve(env, invocation_id="fake-child-id-555")
        self.assertEqual(linkage.invocation_id, "fake-child-id-555")
        self.assertEqual(
            linkage.metadata,
            {
                rel.KEY_PARENT_BUILD_ID: "fake-parent-id-888",
                rel.KEY_PARENT_BUILD_LINK: "http://go/fake-rs/fake-parent-id-888",
                rel.KEY_SIBLING_BUILDS_LINK: "http://go/fake-rs/?q=PARENT_BUILD_ID:fake-parent-id-888",
                rel.KEY_SUB_BUILDS_LINK: "http://go/fake-rs/?q=PARENT_BUILD_ID:fake-child-id-555",
            },
        )

        # Check child environment generated for sub-sub-builds
        self.assertEqual(
            linkage.child_env[rel.ENV_PARENT_BUILD_ID], "fake-child-id-555"
        )
        self.assertEqual(
            linkage.child_env[rel.ENV_PARENT_BUILD_LINK],
            "http://go/fake-rs/fake-child-id-555",
        )
        self.assertEqual(
            linkage.child_env[rel.ENV_RESULTS_URL],
            _FAKE_RESULTS_URL,
        )
        self.assertEqual(
            linkage.child_env[rel.ENV_SIBLING_BUILDS_LINK],
            "http://go/fake-rs/?q=PARENT_BUILD_ID:fake-child-id-555",
        )


if __name__ == "__main__":
    unittest.main()
