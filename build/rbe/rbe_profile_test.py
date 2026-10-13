#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit-tests for rbe_profile.py."""

import os
import sys
import unittest
from pathlib import Path
from unittest import mock

# Insert fuchsia root directory to sys.path so we can import the build packages hermetically
_FUCHSIA_DIR = os.path.dirname(os.path.dirname(os.path.dirname(__file__)))
sys.path.insert(0, _FUCHSIA_DIR)

from build.rbe import rbe_profile


class RbeProfileTest(unittest.TestCase):
    """Tests targeting the RbeProfile dataclass properties, serialization, and loader."""

    def test_rbe_profile_instantiation(self) -> None:
        profile = rbe_profile.RbeProfile(
            rbe_instance="projects/fake-project/instances/fake-instance",
            remote_exec_root=Path("/fake/root"),
            container_image="docker://gcr.io/fake/image",
            worker_pools={
                "default": "Pool=fake-default-pool",
                "large": "Pool=fake-large-pool",
            },
            profile_name="fake-profile",
            os_family="fake-os",
            auth_type="fake-auth",
        )
        self.assertEqual(
            profile.rbe_instance,
            "projects/fake-project/instances/fake-instance",
        )
        self.assertEqual(profile.remote_exec_root, Path("/fake/root"))
        self.assertEqual(profile.container_image, "docker://gcr.io/fake/image")
        self.assertEqual(
            profile.worker_pools,
            {
                "default": "Pool=fake-default-pool",
                "large": "Pool=fake-large-pool",
            },
        )
        self.assertEqual(profile.profile_name, "fake-profile")
        self.assertEqual(profile.os_family, "fake-os")
        self.assertEqual(profile.auth_type, "fake-auth")

    def test_rbe_profile_from_dict_success(self) -> None:
        data = {
            "rbe_instance": "projects/fake-project/instances/default",
            "remote_exec_root": "/fake/root",
            "container_image": "docker://gcr.io/fake/image",
            "worker_pools": {
                "default": "Pool=fake-default-pool",
                "large": "Pool=fake-large-pool",
            },
            "profile_name": "default",
            "os_family": "fake-os",
            "auth_type": "fake-auth",
        }
        profile = rbe_profile.RbeProfile.from_dict(data)
        self.assertEqual(
            profile.rbe_instance, "projects/fake-project/instances/default"
        )
        self.assertEqual(profile.remote_exec_root, Path("/fake/root"))
        self.assertEqual(profile.container_image, "docker://gcr.io/fake/image")
        self.assertEqual(
            profile.worker_pools,
            {
                "default": "Pool=fake-default-pool",
                "large": "Pool=fake-large-pool",
            },
        )
        self.assertEqual(profile.profile_name, "default")
        self.assertEqual(profile.os_family, "fake-os")
        self.assertEqual(profile.auth_type, "fake-auth")

    def test_rbe_profile_from_dict_missing_fields_raises(self) -> None:
        invalid_data = {
            "rbe_instance": "projects/fake-project/instances/fake-instance",
            "remote_exec_root": "/fake/root",
            "container_image": "docker://gcr.io/fake/image",
            # "worker_pools" is missing
            "profile_name": "fake-profile",
            "os_family": "fake-os",
            "auth_type": "fake-auth",
        }
        with self.assertRaises(ValueError) as ctx:
            rbe_profile.RbeProfile.from_dict(invalid_data)
        self.assertIn("worker_pools", str(ctx.exception))

    def test_rbe_profile_from_dict_missing_profile_name_raises(self) -> None:
        invalid_data = {
            "rbe_instance": "projects/fake-project/instances/fake-instance",
            "remote_exec_root": "/fake/root",
            "container_image": "docker://gcr.io/fake/image",
            "worker_pools": {
                "default": "Pool=fake-default-pool",
                "large": "Pool=fake-large-pool",
            },
            # "profile_name" is missing
            "os_family": "fake-os",
            "auth_type": "fake-auth",
        }
        with self.assertRaises(ValueError) as ctx:
            rbe_profile.RbeProfile.from_dict(invalid_data)
        self.assertIn("profile_name", str(ctx.exception))

    def test_rbe_profile_from_dict_missing_container_image_raises(self) -> None:
        invalid_data = {
            "rbe_instance": "projects/fake-project/instances/fake-instance",
            "remote_exec_root": "/fake/root",
            # "container_image" is missing
            "worker_pools": {
                "default": "Pool=fake-default-pool",
                "large": "Pool=fake-large-pool",
            },
            "profile_name": "fake-profile",
            "os_family": "fake-os",
            "auth_type": "fake-auth",
        }
        with self.assertRaises(ValueError) as ctx:
            rbe_profile.RbeProfile.from_dict(invalid_data)
        self.assertIn("container_image", str(ctx.exception))

    def test_rbe_profile_from_dict_missing_os_family_raises(self) -> None:
        invalid_data = {
            "rbe_instance": "projects/fake-project/instances/fake-instance",
            "remote_exec_root": "/fake/root",
            "container_image": "docker://gcr.io/fake/image",
            "worker_pools": {
                "default": "Pool=fake-default-pool",
                "large": "Pool=fake-large-pool",
            },
            "profile_name": "fake-profile",
            # "os_family" is missing
            "auth_type": "fake-auth",
        }
        with self.assertRaises(ValueError) as ctx:
            rbe_profile.RbeProfile.from_dict(invalid_data)
        self.assertIn("os_family", str(ctx.exception))

    def test_rbe_profile_worker_pools_missing_default_raises(self) -> None:
        invalid_data = {
            "rbe_instance": "projects/fake-project/instances/fake-instance",
            "remote_exec_root": "/fake/root",
            "container_image": "docker://gcr.io/fake/image",
            "worker_pools": {"large": "Pool=fake-large-pool"},
            "profile_name": "fake-profile",
            "os_family": "fake-os",
            "auth_type": "fake-auth",
        }
        with self.assertRaises(ValueError) as ctx:
            rbe_profile.RbeProfile.from_dict(invalid_data)
        self.assertIn("must define a 'default' worker pool", str(ctx.exception))

    def test_rbe_profile_worker_pools_missing_large_raises(self) -> None:
        invalid_data = {
            "rbe_instance": "projects/fake-project/instances/fake-instance",
            "remote_exec_root": "/fake/root",
            "container_image": "docker://gcr.io/fake/image",
            "worker_pools": {"default": "Pool=fake-default-pool"},
            "profile_name": "fake-profile",
            "os_family": "fake-os",
            "auth_type": "fake-auth",
        }
        with self.assertRaises(ValueError) as ctx:
            rbe_profile.RbeProfile.from_dict(invalid_data)
        self.assertIn("must define a 'large' worker pool", str(ctx.exception))

    def test_rbe_profile_worker_pools_invalid_platform_variable_raises(
        self,
    ) -> None:
        invalid_data = {
            "rbe_instance": "projects/fake-project/instances/fake-instance",
            "remote_exec_root": "/fake/root",
            "container_image": "docker://gcr.io/fake/image",
            "worker_pools": {
                "default": "no_equals_sign",
                "large": "Pool=fake-large-pool",
            },
            "profile_name": "fake-profile",
            "os_family": "fake-os",
            "auth_type": "fake-auth",
        }
        with self.assertRaises(ValueError) as ctx:
            rbe_profile.RbeProfile.from_dict(invalid_data)
        self.assertIn(
            "must be a 'key=value' platform property string", str(ctx.exception)
        )

    def test_rbe_profile_env_round_trip(self) -> None:
        profile = rbe_profile.RbeProfile(
            rbe_instance="projects/test-project/instances/default",
            remote_exec_root=Path("/test/exec/root"),
            container_image="docker://gcr.io/fake/image",
            worker_pools={
                "default": "Pool=fake-default-pool",
                "large": "Pool=fake-large-pool",
            },
            profile_name="default",
            os_family="fake-os",
            auth_type="fake-auth",
        )
        env_dict = profile.to_env_dict()
        self.assertDictEqual(
            env_dict,
            {
                rbe_profile._ENV_RBE_INSTANCE: "projects/test-project/instances/default",
                rbe_profile._ENV_EXEC_ROOT: "/test/exec/root",
                rbe_profile._ENV_CONTAINER_IMAGE: "docker://gcr.io/fake/image",
                rbe_profile._ENV_OS_FAMILY: "fake-os",
                rbe_profile._ENV_WORKER_POOLS: '{"default": "Pool=fake-default-pool", "large": "Pool=fake-large-pool"}',
                rbe_profile._ENV_AUTH_TYPE: "fake-auth",
                rbe_profile._ENV_PROFILE_NAME: "default",
            },
        )

        # Fast path: load reconstructs this exact profile directly from env without disk I/O
        loaded = rbe_profile.load(Path("/nonexistent"), env_dict)
        self.assertEqual(loaded.rbe_instance, profile.rbe_instance)
        self.assertEqual(loaded.remote_exec_root, profile.remote_exec_root)
        self.assertEqual(loaded.worker_pools, profile.worker_pools)
        self.assertEqual(loaded.auth_type, profile.auth_type)

    @mock.patch.object(rbe_profile._MANAGER, "load")
    def test_load_with_malformed_worker_pools_json(
        self, mock_load: mock.Mock
    ) -> None:
        env_dict = {
            rbe_profile._ENV_RBE_INSTANCE: "projects/env-project/instances/env-rbe",
            rbe_profile._ENV_EXEC_ROOT: "/env/exec/root",
            rbe_profile._ENV_WORKER_POOLS: "{malformed_json",
        }
        with self.assertRaises(ValueError) as ctx:
            rbe_profile.load(Path("/fake"), env_dict)
        self.assertIn(
            "Failed to decode worker pools JSON from environment",
            str(ctx.exception),
        )
        mock_load.assert_not_called()

    def test_load_with_relative_exec_root_raises(self) -> None:
        env_dict = {
            rbe_profile._ENV_RBE_INSTANCE: "projects/env-project/instances/env-rbe",
            rbe_profile._ENV_EXEC_ROOT: "relative/path",
        }
        with self.assertRaises(ValueError) as ctx:
            rbe_profile.load(Path("/fake"), env_dict)
        self.assertIn("must be an absolute path", str(ctx.exception))

    @mock.patch.object(rbe_profile._MANAGER, "load")
    def test_load_delegates_to_manager(self, mock_load: mock.Mock) -> None:
        expected = rbe_profile.fake()
        mock_load.return_value = expected
        loaded = rbe_profile.load(Path("/fake"), {})
        self.assertEqual(loaded, expected)
        mock_load.assert_called_once_with(Path("/fake"), {}, name=None)

    def test_fake(self) -> None:
        # Default fake
        p = rbe_profile.fake()
        self.assertEqual(
            p.rbe_instance, "projects/fake-project/instances/fake-instance"
        )
        self.assertEqual(p.remote_exec_root, Path("/fake/root"))
        self.assertEqual(p.container_image, "docker://gcr.io/fake/image")
        self.assertEqual(
            p.worker_pools,
            {
                "default": "Pool=fake-default-pool",
                "large": "Pool=fake-large-pool",
            },
        )
        self.assertEqual(p.profile_name, "fake-profile")
        self.assertEqual(p.os_family, "fake-os")
        self.assertEqual(p.auth_type, "fake-auth")

        # Overrides
        p_custom = rbe_profile.fake(
            rbe_instance="projects/custom-project/instances/custom-inst",
            auth_type="google",
        )
        self.assertEqual(
            p_custom.rbe_instance,
            "projects/custom-project/instances/custom-inst",
        )
        self.assertEqual(p_custom.auth_type, "google")

        # Unknown override raises ValueError
        with self.assertRaises(ValueError) as ctx:
            rbe_profile.fake(nonexistent_key=True)
        self.assertIn("nonexistent_key", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
