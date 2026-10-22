#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit-tests for remote_services_utils.py functions."""

import os
import sys
import tempfile
import typing as T
import unittest
from pathlib import Path

# Insert the fuchsia root directory to PYTHONPATH so that our custom
# package imports like `from build.rbe import rbe_profile` resolve correctly in tests.
_FUCHSIA_DIR = os.path.dirname(
    os.path.dirname(os.path.dirname(os.path.dirname(__file__)))
)
sys.path.insert(0, _FUCHSIA_DIR)

_SCRIPT_DIR = os.path.dirname(__file__)
sys.path.insert(0, _SCRIPT_DIR)
import remote_services_utils
from build.rbe import rbe_profile, rbe_settings
from build.resultstore import rs_profile


class RemoteServicesUtilsTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self._dir = Path(self._td.name)
        self.source_dir = self._dir / "source"
        self.source_dir.mkdir()

        self.cfg_file1 = (
            self.source_dir / remote_services_utils.RBE_CONFIG_FILES[0]
        )
        self.cfg_file2 = (
            self.source_dir / remote_services_utils.RBE_CONFIG_FILES[1]
        )

        self.cfg_file1.parent.mkdir(parents=True)
        self.cfg_file1.write_text(
            "\n" + "# foo=bar\n"  # empty line  # comment line should be ignored
            "instance=projects/test_project_name/instances/test_instance_name\n"
        )

        self.cfg_file2.parent.mkdir(exist_ok=True)
        self.cfg_file2.write_text(
            "platform=container-image=docker://gcr.io/test_project/debian11,something_else\n"
        )

        # Write fake RBE and ResultStore profiles inside the temporary source dir
        rbe_config = self.source_dir / ".fx/config/rbe_profile"
        rbe_config.parent.mkdir(parents=True, exist_ok=True)
        rbe_config.write_text("default")

        rbe_profile_json = self.source_dir / "build/rbe/profiles/default.json"
        rbe_profile_json.parent.mkdir(parents=True, exist_ok=True)
        rbe_profile_json.write_text(
            """{
  "profile_name": "default",
  "rbe_instance": "projects/test_project_name/instances/test_instance_name",
  "remote_exec_root": "/fake/exec/root",
  "container_image": "docker://gcr.io/fake/image",
  "os_family": "fake-os",
  "worker_pools": {
    "default": "Pool=fake-default-pool",
    "large": "Pool=fake-large-pool"
  },
  "auth_type": "google"
}"""
        )

        rs_profile_json = (
            self.source_dir / "build/resultstore/profiles/default.json"
        )
        rs_profile_json.parent.mkdir(parents=True, exist_ok=True)
        rs_profile_json.write_text(
            """{
  "profile_name": "default",
  "rs_instance": "projects/test_rs_project/instances/test_rs_instance",
  "cas_instance": "projects/test_cas_project/instances/test_cas_instance",
  "results_url": "http://fake-results-url",
  "auth_type": "google"
}"""
        )

    def tearDown(self) -> None:
        self._td.cleanup()

    def test_generate_rbe_config(self) -> None:
        config_dict, input_files = remote_services_utils.generate_rbe_config(
            self.source_dir, environ={}
        )
        self.assertDictEqual(
            config_dict,
            {
                "instance": "projects/test_project_name/instances/test_instance_name",
                "rbe_instance": "projects/test_project_name/instances/test_instance_name",
                "container_image": "docker://gcr.io/fake/image",
                "rs_instance": "projects/test_rs_project/instances/test_rs_instance",
                "rs_project": "test_rs_project",
                "cas_instance": "projects/test_cas_project/instances/test_cas_instance",
                "results_url": "http://fake-results-url",
                "platform": "container-image=docker://gcr.io/test_project/debian11,something_else",
            },
        )

    def test_generate_rbe_config_with_env_bypass(self) -> None:
        # Simulate Option C production path using explicit environment dictionary
        mock_env = {
            "RBE_instance": "projects/env-project/instances/env-rbe",
            "FX_BUILD_INTERNAL_RBE_REMOTE_EXEC_ROOT": "/env/exec/root",
            "FX_BUILD_INTERNAL_RBE_CONTAINER_IMAGE": "docker://gcr.io/fake/image",
            "FX_BUILD_INTERNAL_RBE_OS_FAMILY": "fake-os",
            "FX_BUILD_INTERNAL_RBE_WORKER_POOLS": '{"default": "Pool=env-default", "large": "Pool=env-large"}',
            "RS_rs_instance": "projects/env-project/instances/env-rs",
            "RS_cas_instance": "projects/env-project/instances/env-cas",
            "RS_results_url": "http://env-results/",
        }
        (
            config_dict,
            input_files,
        ) = remote_services_utils.generate_rbe_config(
            self.source_dir, environ=mock_env
        )
        self.assertDictEqual(
            config_dict,
            {
                "instance": "projects/test_project_name/instances/test_instance_name",
                "rbe_instance": "projects/env-project/instances/env-rbe",
                "container_image": "docker://gcr.io/fake/image",
                "rs_instance": "projects/env-project/instances/env-rs",
                "rs_project": "env-project",
                "cas_instance": "projects/env-project/instances/env-cas",
                "results_url": "http://env-results",
                "platform": "container-image=docker://gcr.io/test_project/debian11,something_else",
            },
        )

    def test_generate_rbe_template_substitutions(self) -> None:
        _TEST_CASES: list[dict[str, T.Any]] = [
            {
                "name": "simple_project",
                "config_dict": {
                    "rbe_instance": "projects/test_project1/instances/1",
                    "rs_instance": "projects/test_rs_project/instances/1",
                    "rs_project": "test_rs_project",
                    "cas_instance": "projects/test_cas_project/instances/1",
                    "results_url": "http://fake-results-url",
                    "platform": "foo=bar,container-image=docker://something/else,zoo=qux",
                },
                "remote_download_outputs": "all",
                "expected": {
                    "remote_download_outputs": "all",
                    "remote_instance_name": "projects/test_project1/instances/1",
                    "rs_instance": "projects/test_rs_project/instances/1",
                    "rs_project": "test_rs_project",
                    "cas_instance": "projects/test_cas_project/instances/1",
                    "results_url": "http://fake-results-url",
                    "container_image": "docker://something/else",
                },
            },
        ]
        for test_case in _TEST_CASES:
            msg = f'For {test_case["name"]} case'
            result = remote_services_utils.generate_rbe_template_substitutions(
                test_case["config_dict"], test_case["remote_download_outputs"]
            )
            self.assertDictEqual(result, test_case["expected"], msg=msg)

    def test_generate_remote_services_bazelrc(self) -> None:
        build_dir = self._dir / "my_build_dir"
        build_dir.mkdir(parents=True, exist_ok=True)
        output_file = (
            build_dir / "regenerator_outputs" / "remote_services.bazelrc"
        )
        output_file.parent.mkdir(parents=True, exist_ok=True)

        fake_settings = rbe_settings.fake(
            bazel_enable=True,
            cxx_enable=True,
            link_enable=True,
            rust_enable=True,
            needs_reproxy=True,
            needs_auth=True,
        )

        template_file = (
            self.source_dir / remote_services_utils.RBE_TEMPLATE_FILE
        )
        template_file.parent.mkdir(parents=True, exist_ok=True)
        template_file.write_text(
            r"""# AUTO-GENERATED
my_download_outputs={remote_download_outputs}
my_remote_instance_name={remote_instance_name}
my_rs_instance={rs_instance}
my_rs_project={rs_project}
my_cas_instance={cas_instance}
my_results_url={results_url}
my_container_image={container_image}
"""
        )

        environ: dict[str, str] = {}
        input_files = remote_services_utils.generate_remote_services_bazelrc(
            fuchsia_dir=self.source_dir,
            build_dir=build_dir,
            output_path=output_file,
            download_outputs="find-me",
            environ=environ,
            settings=fake_settings,
        )

        self.assertEqual(
            output_file.read_text(),
            """# AUTO-GENERATED
my_download_outputs=find-me
my_remote_instance_name=projects/test_project_name/instances/test_instance_name
my_rs_instance=projects/test_rs_project/instances/test_rs_instance
my_rs_project=test_rs_project
my_cas_instance=projects/test_cas_project/instances/test_cas_instance
my_results_url=http://fake-results-url
my_container_image=docker://gcr.io/fake/image
""",
        )

        # Resolve the active profile path objects expected in input_files
        rbe_paths = rbe_profile.resolve_paths(self.source_dir, environ)
        rs_paths = rs_profile.resolve_paths(self.source_dir, environ)

        expected_inputs = {
            template_file,
            self.cfg_file1,
            self.cfg_file2,
            *rbe_paths.active_inputs,
            *rs_paths.active_inputs,
        }
        self.assertSetEqual(input_files, expected_inputs)

    def test_generate_remote_services_bazelrc_environ_profile_override(
        self,
    ) -> None:
        build_dir = self._dir / "my_build_dir"
        build_dir.mkdir(parents=True, exist_ok=True)
        output_file = (
            build_dir / "regenerator_outputs" / "remote_services.bazelrc"
        )
        output_file.parent.mkdir(parents=True, exist_ok=True)

        fake_settings = rbe_settings.fake(
            bazel_enable=True,
            cxx_enable=True,
            link_enable=True,
            rust_enable=True,
            needs_reproxy=True,
            needs_auth=True,
        )

        template_file = (
            self.source_dir / remote_services_utils.RBE_TEMPLATE_FILE
        )
        template_file.parent.mkdir(parents=True, exist_ok=True)
        template_file.write_text("my_results_url={results_url}\n")

        # Create distinct custom profile files for RBE and ResultStore disambiguation
        custom_rbe_json = self.source_dir / "build/rbe/profiles/custom_rbe.json"
        custom_rbe_json.parent.mkdir(parents=True, exist_ok=True)
        custom_rbe_json.write_text(
            """{
  "profile_name": "custom_rbe",
  "rbe_instance": "projects/custom-rbe/instances/default",
  "remote_exec_root": "/fake",
  "container_image": "docker://gcr.io/fake/image",
  "os_family": "fake-os",
  "worker_pools": {
    "default": "Pool=custom-pool",
    "large": "Pool=custom-large"
  },
  "auth_type": "google"
}"""
        )

        custom_rs_json = (
            self.source_dir / "build/resultstore/profiles/custom_rs.json"
        )
        custom_rs_json.parent.mkdir(parents=True, exist_ok=True)
        custom_rs_json.write_text(
            """{
  "profile_name": "custom_rs",
  "rs_instance": "projects/custom-rs/instances/default",
  "cas_instance": "projects/custom-rs/instances/default",
  "results_url": "http://custom-rs-results",
  "auth_type": "google"
}"""
        )

        env_override = {
            "FX_BUILD_RBE_PROFILE": "custom_rbe",
            "FX_BUILD_RS_PROFILE": "custom_rs",
        }

        input_files = remote_services_utils.generate_remote_services_bazelrc(
            fuchsia_dir=self.source_dir,
            build_dir=build_dir,
            output_path=output_file,
            download_outputs="all",
            environ=env_override,
            settings=fake_settings,
        )

        rbe_paths = rbe_profile.resolve_paths(self.source_dir, env_override)
        rs_paths = rs_profile.resolve_paths(self.source_dir, env_override)

        for path in rbe_paths.active_inputs:
            self.assertIn(path, input_files)
        for path in rs_paths.active_inputs:
            self.assertIn(path, input_files)
        self.assertIn(custom_rbe_json, input_files)
        self.assertIn(custom_rs_json, input_files)

    def test_generate_remote_services_bazelrc_rbe_disabled(self) -> None:
        # Create a build-like directory structure inside temp directory
        build_dir = self._dir / "my_build_dir"
        build_dir.mkdir(parents=True, exist_ok=True)

        output_file = (
            build_dir / "regenerator_outputs" / "remote_services.bazelrc"
        )
        output_file.parent.mkdir(parents=True, exist_ok=True)

        template_file = (
            self.source_dir / remote_services_utils.RBE_TEMPLATE_FILE
        )
        template_file.parent.mkdir(parents=True, exist_ok=True)
        template_file.write_text(
            r"""# AUTO-GENERATED
my_results_url={results_url}
"""
        )

        fake_settings = rbe_settings.fake(
            bazel_enable=False,
            cxx_enable=False,
            link_enable=False,
            rust_enable=False,
            needs_reproxy=False,
            needs_auth=False,
        )

        input_files = remote_services_utils.generate_remote_services_bazelrc(
            fuchsia_dir=self.source_dir,
            build_dir=build_dir,
            output_path=output_file,
            download_outputs="find-me",
            environ={},
            settings=fake_settings,
        )

        # Since RBE and ResultStore are disabled, the active inputs of RBE and RS profiles should NOT be in input_files!
        rbe_paths = rbe_profile.resolve_paths(self.source_dir, {})
        rs_paths = rs_profile.resolve_paths(self.source_dir, {})

        for path in rbe_paths.active_inputs:
            self.assertNotIn(path, input_files)
        for path in rs_paths.active_inputs:
            self.assertNotIn(path, input_files)

    def test_generate_remote_services_bazelrc_loads_settings_from_build_dir(
        self,
    ) -> None:
        build_dir = self._dir / "my_build_dir2"
        build_dir.mkdir(parents=True, exist_ok=True)
        (build_dir / "rbe_settings.json").write_text(
            """{
  "final": {
    "bazel_enable": true,
    "bazel_exec_strategy": "remote",
    "bazel_download_outputs": "all",
    "cxx_download_objects": true,
    "cxx_enable": true,
    "cxx_exec_strategy": "remote",
    "cxx_minimalist_wrapper": true,
    "link_download_unstripped_outputs": true,
    "link_enable": true,
    "link_exec_strategy": "remote",
    "rust_download_rlibs": true,
    "rust_download_unstripped_binaries": true,
    "rust_enable": true,
    "rust_exec_strategy": "remote",
    "needs_reproxy": true,
    "needs_auth": true
  }
}"""
        )
        output_file = (
            build_dir / "regenerator_outputs" / "remote_services.bazelrc"
        )
        output_file.parent.mkdir(parents=True, exist_ok=True)
        template_file = (
            self.source_dir / remote_services_utils.RBE_TEMPLATE_FILE
        )
        template_file.parent.mkdir(parents=True, exist_ok=True)
        template_file.write_text("my_results_url={results_url}\n")

        # Call with settings=None to exercise rbe_settings.load(build_dir)
        input_files = remote_services_utils.generate_remote_services_bazelrc(
            fuchsia_dir=self.source_dir,
            build_dir=build_dir,
            output_path=output_file,
            download_outputs="all",
            environ={},
        )
        self.assertTrue(output_file.is_file())


if __name__ == "__main__":
    unittest.main()
