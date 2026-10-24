#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit-tests for rs_profile.py."""

import unittest
from pathlib import Path
from unittest import mock

from build.resultstore import rs_profile


class RsProfileTest(unittest.TestCase):
    """Tests targeting the RsProfile dataclass properties, serialization, and loader."""

    def test_rs_profile_instantiation(self) -> None:
        profile = rs_profile.RsProfile(
            rs_instance="projects/fake-project/instances/fake-rs",
            cas_instance="projects/fake-project/instances/fake-cas",
            results_url="http://fake-results-url",
            profile_name="fake-profile",
            auth_type="fake-auth",
        )
        self.assertEqual(
            profile.rs_instance,
            "projects/fake-project/instances/fake-rs",
        )
        self.assertEqual(
            profile.cas_instance,
            "projects/fake-project/instances/fake-cas",
        )
        self.assertEqual(profile.results_url, "http://fake-results-url")
        self.assertEqual(profile.profile_name, "fake-profile")
        self.assertEqual(profile.auth_type, "fake-auth")

    def test_rs_project_validation(self) -> None:
        # Valid format
        p1 = rs_profile.RsProfile(
            rs_instance="projects/valid-project/instances/default",
            cas_instance="projects/valid-project/instances/default",
            results_url="http://fake",
            profile_name="valid",
            auth_type="fake-auth",
        )
        self.assertEqual(p1.rs_project, "valid-project")

        # Invalid format - missing projects prefix
        p2 = rs_profile.RsProfile(
            rs_instance="invalid-project/instances/default",
            cas_instance="invalid-project/instances/default",
            results_url="http://fake",
            profile_name="valid",
            auth_type="fake-auth",
        )
        self.assertEqual(p2.rs_project, "")

        # Invalid format - empty string
        p3 = rs_profile.RsProfile(
            rs_instance="",
            cas_instance="",
            results_url="http://fake",
            profile_name="valid",
            auth_type="fake-auth",
        )
        self.assertEqual(p3.rs_project, "")

    def test_rs_profile_from_dict_success(self) -> None:
        data = {
            "rs_instance": "projects/fake-project/instances/default",
            "cas_instance": "projects/fake-project/instances/default-cas",
            "results_url": "http://fake-results-url/",  # trailing slash should be normalized
            "profile_name": "default",
            "auth_type": "fake-auth",
        }
        profile = rs_profile.RsProfile.from_dict(data)
        self.assertEqual(
            profile.rs_instance, "projects/fake-project/instances/default"
        )
        self.assertEqual(
            profile.cas_instance,
            "projects/fake-project/instances/default-cas",
        )
        self.assertEqual(profile.results_url, "http://fake-results-url")
        self.assertEqual(profile.profile_name, "default")
        self.assertEqual(profile.auth_type, "fake-auth")

    def test_rs_profile_from_dict_missing_fields_raises(self) -> None:
        invalid_data = {
            "rs_instance": "projects/fake-project/instances/fake-instance",
            "cas_instance": "projects/fake-project/instances/fake-cas",
            # "results_url" is missing
            "profile_name": "fake-profile",
            "auth_type": "fake-auth",
        }
        with self.assertRaises(ValueError) as ctx:
            rs_profile.RsProfile.from_dict(invalid_data)
        self.assertIn("results_url", str(ctx.exception))

    def test_rs_profile_from_dict_missing_profile_name_raises(self) -> None:
        invalid_data = {
            "rs_instance": "projects/fake-project/instances/fake-instance",
            "cas_instance": "projects/fake-project/instances/fake-cas",
            "results_url": "http://fake",
            # "profile_name" is missing
            "auth_type": "fake-auth",
        }
        with self.assertRaises(ValueError) as ctx:
            rs_profile.RsProfile.from_dict(invalid_data)
        self.assertIn("profile_name", str(ctx.exception))

    def test_rs_profile_env_round_trip(self) -> None:
        profile = rs_profile.RsProfile(
            rs_instance="projects/test-rs/instances/default",
            cas_instance="projects/test-cas/instances/default",
            results_url="http://fake-results-url/",
            profile_name="default",
            auth_type="fake-auth",
        )
        env_dict = profile.to_env_dict()
        self.assertDictEqual(
            env_dict,
            {
                rs_profile._ENV_RS_INSTANCE: "projects/test-rs/instances/default",
                rs_profile._ENV_CAS_INSTANCE: "projects/test-cas/instances/default",
                rs_profile._ENV_RESULTS_URL: "http://fake-results-url",
                rs_profile._ENV_AUTH_TYPE: "fake-auth",
                rs_profile._ENV_PROFILE_NAME: "default",
            },
        )

        # Test direct from_env_dict deserialization
        reconstructed = rs_profile.RsProfile.from_env_dict(env_dict)
        self.assertEqual(reconstructed, profile)

        # Fast path: load reconstructs this exact profile directly from env without disk I/O
        loaded = rs_profile.load(Path("/nonexistent"), env_dict)
        self.assertEqual(loaded, profile)

    def test_from_env_dict_missing_rs_instance_raises(self) -> None:
        with self.assertRaises(ValueError) as ctx:
            rs_profile.RsProfile.from_env_dict({})
        self.assertIn("RS_rs_instance", str(ctx.exception))

    def test_load_with_missing_cas_or_url_raises(self) -> None:
        env_dict = {
            rs_profile._ENV_RS_INSTANCE: "projects/env-project/instances/env-rs",
        }
        with self.assertRaises(ValueError) as ctx:
            rs_profile.load(Path("/fake"), env_dict)
        self.assertIn("RS_cas_instance", str(ctx.exception))

    @mock.patch.object(rs_profile._MANAGER, "load")
    def test_load_delegates_to_manager(self, mock_load: mock.Mock) -> None:
        expected = rs_profile.fake()
        mock_load.return_value = expected
        loaded = rs_profile.load(Path("/fake"), {})
        self.assertEqual(loaded, expected)
        mock_load.assert_called_once_with(Path("/fake"), {}, name=None)

    def test_fake(self) -> None:
        # Default fake
        p = rs_profile.fake()
        self.assertEqual(
            p.rs_instance, "projects/fake-project/instances/fake-rs"
        )
        self.assertEqual(
            p.cas_instance, "projects/fake-project/instances/fake-cas"
        )
        self.assertEqual(p.results_url, "http://fake-results-url")
        self.assertEqual(p.profile_name, "fake-profile")
        self.assertEqual(p.auth_type, "fake-auth")

        # Overrides
        p_custom = rs_profile.fake(
            rs_instance="custom-rs",
            auth_type="google",
        )
        self.assertEqual(p_custom.rs_instance, "custom-rs")
        self.assertEqual(p_custom.auth_type, "google")

        # Unknown override raises ValueError
        with self.assertRaises(ValueError) as ctx:
            rs_profile.fake(nonexistent_key=True)
        self.assertIn("nonexistent_key", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
