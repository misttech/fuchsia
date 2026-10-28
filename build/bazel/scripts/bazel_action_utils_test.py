#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import sys
import tempfile
import typing as T
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(__file__))
import bazel_build_events
from bazel_action_utils import (
    AspectManifestOutputs,
    parse_build_event_manifests,
)


class BazelTargetInfosMapTest(unittest.TestCase):
    def test_update_rust_project(self) -> None:
        from bazel_action_utils import BazelTargetInfosMap

        sample_content: list[dict[str, T.Any]] = [
            {
                "type": "file",
                "bazel_target": "//src:foo",
                "bazel_platform_label": "//build/bazel/platforms:host",
                "bazel_platform_config": "host",
                "ninja_depfile": "obj/src/foo.d",
                "gn_targets_manifest": "gen/gn_targets.manifest",
                "stamp_path": "obj/src/foo.stamp",
                "bazel_file": "foo",
                "ninja_file": "foo",
                "update_rust_project": True,
            },
            {
                "type": "file",
                "bazel_target": "//src:bar",
                "bazel_platform_label": "//build/bazel/platforms:host",
                "bazel_platform_config": "host",
                "ninja_depfile": "obj/src/bar.d",
                "gn_targets_manifest": "gen/gn_targets.manifest",
                "stamp_path": "obj/src/bar.stamp",
                "bazel_file": "bar",
                "ninja_file": "bar",
                "update_rust_project": False,
            },
        ]
        target_map = BazelTargetInfosMap(sample_content)
        foo_info = target_map.get_info(
            "//src:foo", "//build/bazel/platforms:host"
        )
        self.assertIsNotNone(foo_info)
        assert foo_info is not None
        self.assertTrue(foo_info.update_rust_project)

        bar_info = target_map.get_info(
            "//src:bar", "//build/bazel/platforms:host"
        )
        self.assertIsNotNone(bar_info)
        assert bar_info is not None
        self.assertFalse(bar_info.update_rust_project)

    def test_copy_debug_symbols(self) -> None:
        from bazel_action_utils import BazelTargetInfosMap

        def entry(name: str, **extra: T.Any) -> dict[str, T.Any]:
            return {
                "type": "file",
                "bazel_target": f"//src:{name}",
                "bazel_platform_label": "//build/bazel/platforms:host",
                "bazel_platform_config": "host",
                "ninja_depfile": f"obj/src/{name}.d",
                "gn_targets_manifest": "gen/gn_targets.manifest",
                "stamp_path": f"obj/src/{name}.stamp",
                "bazel_file": name,
                "ninja_file": name,
                "update_rust_project": False,
                **extra,
            }

        target_map = BazelTargetInfosMap(
            [
                entry("foo", copy_debug_symbols=True),
                entry("bar", copy_debug_symbols=False),
                entry("baz"),
            ]
        )
        for name, expected in [("foo", True), ("bar", False), ("baz", False)]:
            info = target_map.get_info(
                f"//src:{name}", "//build/bazel/platforms:host"
            )
            assert info is not None
            self.assertEqual(info.copy_debug_symbols, expected, name)

    def test_extra_bazel_targets_file(self) -> None:
        from bazel_action_utils import BazelTargetInfosMap

        def entry(name: str, **extra: T.Any) -> dict[str, T.Any]:
            return {
                "type": "file",
                "bazel_target": f"//src:{name}",
                "bazel_platform_label": "//build/bazel/platforms:host",
                "bazel_platform_config": "host",
                "ninja_depfile": f"obj/src/{name}.d",
                "gn_targets_manifest": "gen/gn_targets.manifest",
                "stamp_path": f"obj/src/{name}.stamp",
                "bazel_file": name,
                "ninja_file": name,
                "update_rust_project": False,
                **extra,
            }

        target_map = BazelTargetInfosMap(
            [
                entry("foo", extra_bazel_targets_file="extra_targets.txt"),
                entry("bar"),
            ]
        )

        foo_info = target_map.get_info(
            "//src:foo", "//build/bazel/platforms:host"
        )
        assert foo_info is not None
        self.assertEqual(foo_info.extra_bazel_targets_file, "extra_targets.txt")

        # The field is optional, and most actions omit it entirely.
        bar_info = target_map.get_info(
            "//src:bar", "//build/bazel/platforms:host"
        )
        assert bar_info is not None
        self.assertIsNone(bar_info.extra_bazel_targets_file)

        self.assertEqual(list(target_map.all_infos()), [foo_info, bar_info])

    def test_debug_symbols_manifest(self) -> None:
        from bazel_action_utils import BazelTargetInfosMap

        def entry(name: str, **extra: T.Any) -> dict[str, T.Any]:
            return {
                "type": "file",
                "bazel_target": f"//src:{name}",
                "bazel_platform_label": "//build/bazel/platforms:host",
                "bazel_platform_config": "host",
                "ninja_depfile": f"obj/src/{name}.d",
                "gn_targets_manifest": "gen/gn_targets.manifest",
                "stamp_path": f"obj/src/{name}.stamp",
                "bazel_file": name,
                "ninja_file": name,
                "update_rust_project": False,
                **extra,
            }

        target_map = BazelTargetInfosMap(
            [
                entry(
                    "foo",
                    debug_symbols_manifest="obj/src/foo.debug_symbols.json",
                ),
                entry("bar"),
            ]
        )

        foo_info = target_map.get_info(
            "//src:foo", "//build/bazel/platforms:host"
        )
        assert foo_info is not None
        self.assertEqual(
            foo_info.debug_symbols_manifest,
            "obj/src/foo.debug_symbols.json",
        )
        self.assertEqual(
            target_map.get_target("obj/src/foo.debug_symbols.json"),
            foo_info,
        )

        bar_info = target_map.get_info(
            "//src:bar", "//build/bazel/platforms:host"
        )
        assert bar_info is not None
        self.assertIsNone(bar_info.debug_symbols_manifest)

    def test_package_outputs(self) -> None:
        from bazel_action_utils import BazelTargetInfosMap, PackageOutput

        def entry(name: str, **extra: T.Any) -> dict[str, T.Any]:
            return {
                "type": "package",
                "bazel_target": f"//src:{name}",
                "bazel_platform_label": "//build/bazel/platforms:fuchsia",
                "bazel_platform_config": "fuchsia",
                "ninja_depfile": f"obj/src/{name}.d",
                "gn_targets_manifest": "gen/gn_targets.manifest",
                "stamp_path": f"obj/src/{name}.stamp",
                "ninja_archive": f"obj/src/{name}.far",
                "copy_debug_symbols": True,
                "update_rust_project": False,
                **extra,
            }

        target_map = BazelTargetInfosMap(
            [
                entry(
                    "foo",
                    ninja_package_out_dir="obj/src/foo",
                ),
            ]
        )

        foo_info = target_map.get_info(
            "//src:foo", "//build/bazel/platforms:fuchsia"
        )
        assert foo_info is not None
        self.assertEqual(
            foo_info.package_outputs,
            [
                PackageOutput(
                    package_label="//src:foo",
                    archive_path="obj/src/foo.far",
                    copy_debug_symbols=True,
                    package_out_dir="obj/src/foo",
                )
            ],
        )
        self.assertEqual(
            foo_info.package_outputs[0].extracted_ninja_paths(),
            ["obj/src/foo/package_manifest.json", "obj/src/foo/blobs.json"],
        )
        # The extracted files must map back to the Bazel target so that
        # Ninja delayed actions requesting them can be resolved.
        for path in [
            "obj/src/foo.far",
            "obj/src/foo/package_manifest.json",
            "obj/src/foo/blobs.json",
        ]:
            self.assertEqual(target_map.get_target(path), foo_info, path)

        # The extraction fields are required.
        with self.assertRaises(KeyError):
            BazelTargetInfosMap([entry("bar")])


class AspectManifestOutputsTest(unittest.TestCase):
    def test_to_from_dict_and_file(self) -> None:
        outputs = AspectManifestOutputs(
            source_files_manifest_paths=["fake-out/bin/fake_sources.json"],
            debug_symbol_manifest_paths=[
                "fake-out/bin/fake_symbols.debug_symbols.json"
            ],
            rust_analyzer_manifest_paths=[
                "fake-out/bin/fake_rust.fuchsia_rust_analyzer_manifest.json"
            ],
            genquery_output_files=[
                "//buildfiles_genquery:fake_q,fake-out/bin/fake_q.txt"
            ],
        )
        d = outputs.to_dict()
        reconstructed = AspectManifestOutputs.from_dict(d)
        self.assertEqual(outputs, reconstructed)

        with tempfile.TemporaryDirectory() as tmpdir:
            file_path = Path(tmpdir) / "all_manifests.json"
            outputs.save_to_file(file_path)
            loaded = AspectManifestOutputs.load_from_file(file_path)
            self.assertEqual(outputs, loaded)

    def test_from_build_event_stream(self) -> None:
        events = [
            {
                "id": {"namedSet": {"id": "set-src"}},
                "namedSetOfFiles": {
                    "files": [
                        {
                            "name": "fake/pkg/fake_target.fuchsia_source_files.json",
                            "pathPrefix": ["fake-out", "bin"],
                        }
                    ]
                },
            },
            {
                "id": {"namedSet": {"id": "set-dbg"}},
                "namedSetOfFiles": {
                    "files": [
                        {
                            "name": "fake/pkg/fake_bin.debug_symbols.json",
                            "pathPrefix": ["fake-out", "bin"],
                        },
                        {
                            "name": "fake/pkg/fake_bin",
                            "pathPrefix": ["fake-out", "bin"],
                        },
                    ]
                },
            },
            {
                "id": {"namedSet": {"id": "set-rust"}},
                "namedSetOfFiles": {
                    "files": [
                        {
                            "name": "fake/pkg/fake_rust.fuchsia_rust_analyzer_manifest.json",
                            "pathPrefix": ["fake-out", "bin"],
                        }
                    ]
                },
            },
            {
                "id": {"namedSet": {"id": "set-gq"}},
                "namedSetOfFiles": {
                    "files": [
                        {
                            "name": "buildfiles_genquery/fake_target.buildfiles.txt",
                            "pathPrefix": ["fake-out", "bin"],
                        }
                    ]
                },
            },
            {
                "id": {"targetCompleted": {"label": "//fake/pkg:fake_target"}},
                "completed": {
                    "outputGroup": [
                        {
                            "name": "fuchsia_sources_manifest",
                            "fileSets": [{"id": "set-src"}],
                        }
                    ]
                },
            },
            {
                "id": {"targetCompleted": {"label": "//fake/pkg:fake_bin"}},
                "completed": {
                    "outputGroup": [
                        {
                            "name": "debug_symbol_manifest",
                            "fileSets": [{"id": "set-dbg"}],
                        }
                    ]
                },
            },
            {
                "id": {"targetCompleted": {"label": "//fake/pkg:fake_rust"}},
                "completed": {
                    "outputGroup": [
                        {
                            "name": "fuchsia_rust_analyzer_manifest",
                            "fileSets": [{"id": "set-rust"}],
                        }
                    ]
                },
            },
            {
                "id": {
                    "targetCompleted": {
                        "label": "//buildfiles_genquery:fake_target.buildfiles.txt"
                    }
                },
                "completed": {
                    "outputGroup": [
                        {
                            "name": "default",
                            "fileSets": [{"id": "set-gq"}],
                        }
                    ]
                },
            },
        ]
        stream = bazel_build_events.BuildEventStream.from_events(events)
        outputs = AspectManifestOutputs.from_build_event_stream(stream)

        expected_src = os.path.join(
            "fake-out", "bin", "fake/pkg/fake_target.fuchsia_source_files.json"
        )
        expected_dbg = os.path.join(
            "fake-out", "bin", "fake/pkg/fake_bin.debug_symbols.json"
        )
        expected_rust = os.path.join(
            "fake-out",
            "bin",
            "fake/pkg/fake_rust.fuchsia_rust_analyzer_manifest.json",
        )
        expected_gq = os.path.join(
            "fake-out", "bin", "buildfiles_genquery/fake_target.buildfiles.txt"
        )

        self.assertEqual(outputs.source_files_manifest_paths, [expected_src])
        self.assertEqual(
            outputs.debug_symbol_manifest_paths,
            [f"@@//fake/pkg:fake_bin,{expected_dbg}"],
        )
        self.assertEqual(outputs.rust_analyzer_manifest_paths, [expected_rust])
        self.assertEqual(
            outputs.genquery_output_files,
            [
                f"@@//buildfiles_genquery:fake_target.buildfiles.txt,{expected_gq}"
            ],
        )

    def test_parse_build_event_manifests_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            bep_path = Path(tmpdir) / "bep.json"
            bep_path.write_text(
                json.dumps(
                    {
                        "id": {"namedSet": {"id": "set-1"}},
                        "namedSetOfFiles": {
                            "files": [
                                {
                                    "name": "fake/target.fuchsia_source_files.json",
                                    "pathPrefix": ["fake-out", "bin"],
                                }
                            ]
                        },
                    }
                )
                + "\n"
                + json.dumps(
                    {
                        "id": {"targetCompleted": {"label": "//fake:target"}},
                        "completed": {
                            "outputGroup": [
                                {
                                    "name": "fuchsia_sources_manifest",
                                    "fileSets": [{"id": "set-1"}],
                                }
                            ]
                        },
                    }
                )
                + "\n"
            )
            outputs = parse_build_event_manifests(bep_path)
            expected = os.path.join(
                "fake-out", "bin", "fake/target.fuchsia_source_files.json"
            )
            self.assertEqual(outputs.source_files_manifest_paths, [expected])

    def test_buildfiles_genquery_filtering(self) -> None:
        events = [
            {
                "id": {"namedSet": {"id": "set-debug"}},
                "namedSetOfFiles": {
                    "files": [
                        {
                            "name": "fake/real.debug_symbols.json",
                            "pathPrefix": ["fake-out", "bin"],
                        },
                        {
                            "name": "buildfiles_genquery/query.debug_symbols.json",
                            "pathPrefix": ["fake-out", "bin"],
                        },
                    ]
                },
            },
            {
                "id": {"targetCompleted": {"label": "//fake:target"}},
                "completed": {
                    "outputGroup": [
                        {
                            "name": "debug_symbol_manifest",
                            "fileSets": [{"id": "set-debug"}],
                        }
                    ]
                },
            },
        ]
        stream = bazel_build_events.BuildEventStream.from_events(events)
        outputs = AspectManifestOutputs.from_build_event_stream(stream)
        expected = os.path.join(
            "fake-out", "bin", "fake/real.debug_symbols.json"
        )
        self.assertEqual(
            outputs.debug_symbol_manifest_paths, [f"@@//fake:target,{expected}"]
        )


if __name__ == "__main__":
    unittest.main()
