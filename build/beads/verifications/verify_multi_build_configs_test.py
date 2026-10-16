#!/usr/bin/env fuchsia-vendored-python

# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import contextlib
import io
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

_FUCHSIA_DIR = pathlib.Path(__file__).parent.parent.parent.parent
sys.path.insert(0, str(_FUCHSIA_DIR / "build/beads/scripts"))
sys.path.insert(0, str(_FUCHSIA_DIR / "build/beads/verifications"))

import compare_utils
import verify_build_commands
import verify_multi_build_configs


class VerifyMultiBuildConfigsTest(unittest.TestCase):
    def test_parse_and_generate_args_gn_standard(self) -> None:
        textproto = """
# Auto-generated fint params
compilation_mode: COMPILATION_MODE_RELEASE
board: "boards/x64.gni"
product: "products/core.gni"
gn_args: "build_should_trace_actions = true"
gn_args: "truncate_build_info_commit_date = true"
gn_args: "universe_package_labels += [ \\"//foo:bar\\" ]"
gn_args: "cxx_rbe_enable = true"
target_arch: X64
universe_packages: "//bundles/buildbot/core"
rust_rbe_enable: true
cxx_rbe_enable: true
"""
        cfg = verify_multi_build_configs.parse_fint_params_textproto(
            textproto,
            name="core.x64-release",
            source_path=pathlib.Path(
                "fint_params/global.ci/core.x64-release.textproto"
            ),
        )
        self.assertEqual(cfg.name, "core.x64-release")
        self.assertEqual(cfg.board, "boards/x64.gni")
        self.assertEqual(cfg.product, "products/core.gni")
        self.assertEqual(cfg.compilation_mode, "release")
        self.assertEqual(cfg.target_cpu, "x64")
        self.assertEqual(cfg.variants, [])

        args_gn = cfg.generate_args_gn()
        self.assertEqual(
            args_gn,
            """# Generated from fint_params/global.ci/core.x64-release.textproto

import("//boards/x64.gni")
import("//products/core.gni")

# Basic args:
build_info_board = "x64"
build_info_product = "core"
build_only_labels = ["//build/beads/verifications:example_targets"]
compilation_mode = "release"
export_bazel_tests = false
target_cpu = "x64"
truncate_build_info_commit_date = true
""",
        )

    def test_parse_and_generate_args_gn_with_variants_and_scopes(self) -> None:
        textproto = """
compilation_mode: COMPILATION_MODE_DEBUG
board: "boards/arm64.gni"
product: "products/core.gni"
gn_args: "optimize = \\"sanitizer\\""
target_arch: ARM64
variants: "asan-ubsan"
variants: "host_asan-ubsan"
variants: "{variant=\\"asan-fuzzer\\" target_type=[\\"fuzzed_executable\\"]}"
"""
        cfg = verify_multi_build_configs.parse_fint_params_textproto(
            textproto,
            name="core.arm64-asan",
        )
        self.assertEqual(
            cfg.variants,
            [
                "asan-ubsan",
                "host_asan-ubsan",
                '{variant="asan-fuzzer" target_type=["fuzzed_executable"]}',
            ],
        )
        args_gn = cfg.generate_args_gn(
            extra_build_only_labels=["//src/my_target:bin"]
        )
        self.assertIn(
            'select_variant = ["asan-ubsan", "host_asan-ubsan", {variant="asan-fuzzer" target_type=["fuzzed_executable"]}]',
            args_gn,
        )
        self.assertIn('optimize = "sanitizer"', args_gn)
        self.assertIn(
            'build_only_labels = ["//build/beads/verifications:example_targets", "//src/my_target:bin"]',
            args_gn,
        )

    def test_extract_extra_build_only_labels(self) -> None:
        queries = [
            compare_utils.CompareCommandsQuery.from_dict(
                {
                    "gn": "//build/beads/verifications/rust:beads_rust_example_bin.actual(//build/toolchain:host_x64)",
                    "bazel": "//build/beads/verifications/rust:beads_rust_example_bin",
                    "bazel_config": "host",
                    "type": "rustc",
                }
            ),
            compare_utils.CompareCommandsQuery.from_dict(
                {
                    "gn": "//src/sys/foo:bar_rust.actual",
                    "bazel": "//src/sys/foo:bar_rust",
                    "bazel_config": "fuchsia_platform",
                    "type": "rustc",
                }
            ),
            compare_utils.CompareCommandsQuery.from_dict(
                {
                    "gn": "//src/sys/foo:bar_cc(//build/toolchain:host_x64)",
                    "bazel": "//src/sys/foo:bar_cc",
                    "bazel_config": "host",
                    "type": "cpp_compile",
                }
            ),
        ]
        extra = verify_multi_build_configs.extract_extra_build_only_labels(
            queries
        )
        self.assertEqual(
            extra,
            [
                "//src/sys/foo:bar_rust",
                "//src/sys/foo:bar_cc(//build/toolchain:host_x64)",
            ],
        )

    def test_deduplicate_configs(self) -> None:
        c1 = verify_multi_build_configs.FintParamsConfig(
            name="core.x64-debug",
            board="boards/x64.gni",
            product="products/core.gni",
            compilation_mode="debug",
            target_cpu="x64",
            gn_args=["build_should_trace_actions = true"],
        )
        c2 = verify_multi_build_configs.FintParamsConfig(
            name="core.x64-debug-ctf",
            board="boards/x64.gni",
            product="products/core.gni",
            compilation_mode="debug",
            target_cpu="x64",
            gn_args=["use_test_pilot = true"],
        )
        unique, aliases = verify_multi_build_configs.deduplicate_configs(
            [c1, c2]
        )
        self.assertEqual(len(unique), 1)
        self.assertEqual(unique[0].name, "core.x64-debug")
        self.assertEqual(
            aliases["core.x64-debug"], ["core.x64-debug", "core.x64-debug-ctf"]
        )

    def test_resolve_gn_label_for_toolchains(self) -> None:
        # Non-variant build: labels remain unchanged.
        non_variant_tc = {
            "default_toolchain": "//build/toolchain/fuchsia:x64",
            "fuchsia_toolchain": "//build/toolchain/fuchsia:x64",
            "host_toolchain": "//build/toolchain:host_x64",
            "host_variant_toolchain": "//build/toolchain:host_x64",
        }
        self.assertEqual(
            verify_build_commands.resolve_gn_label_for_toolchains(
                "//build/beads/verifications/cc:beads_cc_example_bin",
                non_variant_tc,
            ),
            "//build/beads/verifications/cc:beads_cc_example_bin",
        )
        self.assertEqual(
            verify_build_commands.resolve_gn_label_for_toolchains(
                "//build/beads/verifications/cc:beads_cc_example_bin(//build/toolchain:host_x64)",
                non_variant_tc,
            ),
            "//build/beads/verifications/cc:beads_cc_example_bin(//build/toolchain:host_x64)",
        )

        # Variant build (e.g., arm64-asan): both Fuchsia and Host toolchains redirect.
        variant_tc = {
            "default_toolchain": "//build/toolchain/fuchsia:arm64",
            "fuchsia_toolchain": "//build/toolchain/fuchsia:arm64-asan-ubsan",
            "host_toolchain": "//build/toolchain:host_x64",
            "host_variant_toolchain": "//build/toolchain:host_x64-asan-ubsan",
        }
        self.assertEqual(
            verify_build_commands.resolve_gn_label_for_toolchains(
                "//build/beads/verifications/cc:beads_cc_example_bin",
                variant_tc,
            ),
            "//build/beads/verifications/cc:beads_cc_example_bin(//build/toolchain/fuchsia:arm64-asan-ubsan)",
        )
        self.assertEqual(
            verify_build_commands.resolve_gn_label_for_toolchains(
                "//build/beads/verifications/cc:beads_cc_example_bin(//build/toolchain:host_x64)",
                variant_tc,
            ),
            "//build/beads/verifications/cc:beads_cc_example_bin(//build/toolchain:host_x64-asan-ubsan)",
        )
        # Explicit non-default host toolchains should not be overridden.
        self.assertEqual(
            verify_build_commands.resolve_gn_label_for_toolchains(
                "//build/beads/verifications/cc:beads_cc_example_bin(//build/toolchain:host_x64_custom)",
                variant_tc,
            ),
            "//build/beads/verifications/cc:beads_cc_example_bin(//build/toolchain:host_x64_custom)",
        )

    def test_resolve_queries_for_build_dir(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            build_dir = pathlib.Path(tmpdir)
            (
                build_dir / verify_build_commands.BEADS_TOOLCHAINS_FILENAME
            ).write_text(
                json.dumps(
                    {
                        "default_toolchain": "//build/toolchain/fuchsia:x64",
                        "fuchsia_toolchain": "//build/toolchain/fuchsia:x64-lto",
                        "host_toolchain": "//build/toolchain:host_x64",
                        "host_variant_toolchain": "//build/toolchain:host_x64-novariant",
                    }
                )
            )
            q_fuchsia = compare_utils.CompareCommandsQuery.from_dict(
                {
                    "gn": "//build/beads/verifications/cc:beads_cc_example_bin",
                    "bazel": "//build/beads/verifications/cc:beads_cc_example_bin",
                    "bazel_config": "fuchsia_platform",
                    "type": "cpp_link",
                }
            )
            q_host = compare_utils.CompareCommandsQuery.from_dict(
                {
                    "gn": "//build/beads/verifications/cc:beads_cc_example_bin(//build/toolchain:host_x64)",
                    "bazel": "//build/beads/verifications/cc:beads_cc_example_bin",
                    "bazel_config": "host",
                    "type": "cpp_link",
                }
            )
            resolved = verify_build_commands.resolve_queries_for_build_dir(
                [q_fuchsia, q_host], build_dir
            )
            self.assertEqual(
                resolved[0].gn,
                "//build/beads/verifications/cc:beads_cc_example_bin(//build/toolchain/fuchsia:x64-lto)",
            )
            self.assertEqual(
                resolved[1].gn,
                "//build/beads/verifications/cc:beads_cc_example_bin(//build/toolchain:host_x64-novariant)",
            )

    def test_dry_run_and_filters(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = pathlib.Path(tmpdir)
            fint_dir = (
                root
                / "integration/infra/config/generated/fuchsia/fint_params/global.ci"
            )
            fint_dir.mkdir(parents=True)
            (fint_dir / "core.x64-debug.textproto").write_text(
                'compilation_mode: COMPILATION_MODE_DEBUG\nboard: "boards/x64.gni"\nproduct: "products/core.gni"\ntarget_arch: X64\n'
            )
            (fint_dir / "core.arm64-asan.textproto").write_text(
                'compilation_mode: COMPILATION_MODE_BALANCED\nboard: "boards/arm64.gni"\nproduct: "products/core.gni"\ntarget_arch: ARM64\nvariants: "asan-ubsan"\nvariants: "host_asan-ubsan"\n'
            )
            (fint_dir / "core.arm64-hwasan.textproto").write_text(
                'compilation_mode: COMPILATION_MODE_BALANCED\nboard: "boards/arm64.gni"\nproduct: "products/core.gni"\ntarget_arch: ARM64\nvariants: "host_asan-ubsan"\nvariants: "hwasan-ubsan"\n'
            )
            (fint_dir / "core.x64-lto.textproto").write_text(
                'compilation_mode: COMPILATION_MODE_RELEASE\nboard: "boards/x64.gni"\nproduct: "products/core.gni"\ntarget_arch: X64\nvariants: "lto"\n'
            )
            (fint_dir / "core.x64-thinlto.textproto").write_text(
                'compilation_mode: COMPILATION_MODE_RELEASE\nboard: "boards/x64.gni"\nproduct: "products/core.gni"\ntarget_arch: X64\nvariants: "thinlto"\n'
            )
            manifest = root / "targets.json"
            manifest.write_text("[]")
            build_dir = root / "out/default"

            buf = io.StringIO()
            with contextlib.redirect_stdout(buf):
                rc = verify_multi_build_configs.main(
                    [
                        "--fuchsia-dir",
                        str(root),
                        "--build-dir",
                        str(build_dir),
                        "--manifest",
                        str(manifest),
                        "--fint-params",
                        "core.x64-debug",
                        "--fint-params",
                        "core.arm64-asan",
                        "--fint-params",
                        "core.arm64-hwasan",
                        "--arch",
                        "arm64",
                        "--variant",
                        "asan",
                        "--dry-run",
                    ]
                )
            self.assertEqual(rc, 0)
            out = buf.getvalue()
            self.assertIn("[1/1] core.arm64-asan", out)
            self.assertIn("-> out/default/beads_verify_core.arm64-asan", out)
            self.assertNotIn("core.x64-debug", out)
            self.assertNotIn("core.arm64-hwasan", out)
            self.assertFalse(build_dir.exists())

            # Verify --variant lto does not match thinlto.
            buf_lto = io.StringIO()
            with contextlib.redirect_stdout(buf_lto):
                rc = verify_multi_build_configs.main(
                    [
                        "--fuchsia-dir",
                        str(root),
                        "--build-dir",
                        str(build_dir),
                        "--manifest",
                        str(manifest),
                        "--fint-params",
                        "core.x64-lto",
                        "--fint-params",
                        "core.x64-thinlto",
                        "--variant",
                        "lto",
                        "--dry-run",
                    ]
                )
            self.assertEqual(rc, 0)
            out_lto = buf_lto.getvalue()
            self.assertIn("[1/1] core.x64-lto", out_lto)
            self.assertNotIn("core.x64-thinlto", out_lto)

            cfg = verify_multi_build_configs.FintParamsConfig(
                name="core.x64-debug",
                board="boards/x64.gni",
                product="products/core.gni",
                compilation_mode="debug",
                target_cpu="x64",
            )
            res = verify_multi_build_configs.ConfigVerificationResult(
                config=cfg,
                build_dir=build_dir / "beads_verify_core.x64-debug",
                duration_sec=1.2,
                regenerated=False,
            )
            report = verify_multi_build_configs.format_summary_report(
                [res], {"core.x64-debug": ["core.x64-debug"]}, fuchsia_dir=root
            )
            self.assertIn(
                "| `out/default/beads_verify_core.x64-debug` |", report
            )

    def test_select_queries_for_configs(self) -> None:
        q_fuchsia = compare_utils.CompareCommandsQuery.from_dict(
            {
                "gn": "//build/beads/verifications/cc:beads_cc_example_bin",
                "bazel": "//build/beads/verifications/cc:beads_cc_example_bin",
                "bazel_config": "fuchsia_platform",
                "type": "cpp_link",
            }
        )
        q_host = compare_utils.CompareCommandsQuery.from_dict(
            {
                "gn": "//build/beads/verifications/cc:beads_cc_example_bin(//build/toolchain:host_x64)",
                "bazel": "//build/beads/verifications/cc:beads_cc_example_bin",
                "bazel_config": "host",
                "type": "cpp_link",
            }
        )
        c1 = verify_multi_build_configs.FintParamsConfig(
            name="core.x64-release",
            compilation_mode="release",
            target_cpu="x64",
        )
        c2 = verify_multi_build_configs.FintParamsConfig(
            name="core.arm64-release",
            compilation_mode="release",
            target_cpu="arm64",
        )
        c3 = verify_multi_build_configs.FintParamsConfig(
            name="core.x64-lto",
            compilation_mode="release",
            target_cpu="x64",
            variants=["lto"],
        )
        c4 = verify_multi_build_configs.FintParamsConfig(
            name="core.x64-asan",
            compilation_mode="debug",
            target_cpu="x64",
            variants=["asan-ubsan", "host_asan-ubsan"],
        )

        selected = verify_multi_build_configs.select_queries_for_configs(
            [c1, c2, c3, c4], [q_fuchsia, q_host], dedup_host_queries=True
        )
        # c1 is first with (release, (), ()) -> gets both fuchsia and host
        self.assertEqual(len(selected[0]), 2)
        # c2 and c3 share the same host signature as c1 -> only fuchsia query
        self.assertEqual(selected[1], [q_fuchsia])
        self.assertEqual(selected[2], [q_fuchsia])
        # c4 has a new host signature (debug, ('host_asan-ubsan',), ()) -> gets both
        self.assertEqual(len(selected[3]), 2)

        host_cmp = compare_utils.CompareCommandsResult(
            query=q_host,
            normalized_gn_args=["clang++", "-O2"],
            normalized_bazel_args=["clang++", "-O3"],
        )
        fuchsia_cmp = compare_utils.CompareCommandsResult(
            query=q_fuchsia,
            normalized_gn_args=["clang++", "-O2"],
            normalized_bazel_args=["clang++", "-O2"],
        )
        r1 = verify_multi_build_configs.ConfigVerificationResult(
            config=c1,
            build_dir=pathlib.Path("/tmp/nonexistent1"),
            duration_sec=1.0,
            regenerated=False,
            verification=verify_build_commands.summarize_comparison_results(
                pathlib.Path("/tmp/nonexistent1"), [host_cmp, fuchsia_cmp]
            ),
        )
        r2 = verify_multi_build_configs.ConfigVerificationResult(
            config=c2,
            build_dir=pathlib.Path("/tmp/nonexistent2"),
            duration_sec=1.0,
            regenerated=False,
            verification=verify_build_commands.summarize_comparison_results(
                pathlib.Path("/tmp/nonexistent2"), [fuchsia_cmp]
            ),
        )
        self.assertTrue(r2.passed)
        verify_multi_build_configs.merge_deduped_host_results([r1, r2])
        self.assertFalse(r2.passed)
        assert r2.verification is not None
        self.assertEqual(r2.verification.unexpected_differences_count, 1)

        # If the primary config failed in run_regenerator(), secondary configs
        # that omitted host queries must not silently pass.
        r1_failed = verify_multi_build_configs.ConfigVerificationResult(
            config=c1,
            build_dir=pathlib.Path("/tmp/nonexistent1"),
            duration_sec=1.0,
            regenerated=True,
            regen_error="regenerator.py failed",
        )
        r2_missing_host = verify_multi_build_configs.ConfigVerificationResult(
            config=c2,
            build_dir=pathlib.Path("/tmp/nonexistent2"),
            duration_sec=1.0,
            regenerated=False,
            verification=verify_build_commands.summarize_comparison_results(
                pathlib.Path("/tmp/nonexistent2"), [fuchsia_cmp]
            ),
        )
        verify_multi_build_configs.merge_deduped_host_results(
            [r1_failed, r2_missing_host]
        )
        self.assertFalse(r2_missing_host.passed)
        assert r2_missing_host.verification is not None
        self.assertEqual(len(r2_missing_host.verification.errors), 1)

    def test_host_dedup_progress_and_regen_fallback(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = pathlib.Path(tmpdir)
            fint_dir = (
                root
                / "integration/infra/config/generated/fuchsia/fint_params/global.ci"
            )
            fint_dir.mkdir(parents=True)
            for name, arch in (
                ("core.x64-asan", "X64"),
                ("core.arm64-asan", "ARM64"),
                ("core.arm64-hwasan", "ARM64"),
            ):
                target_var = (
                    "hwasan-ubsan" if "hwasan" in name else "asan-ubsan"
                )
                (fint_dir / f"{name}.textproto").write_text(
                    f'compilation_mode: COMPILATION_MODE_DEBUG\nboard: "boards/{arch.lower()}.gni"\nproduct: "products/core.gni"\ntarget_arch: {arch}\nvariants: "{target_var}"\nvariants: "host_asan-ubsan"\n'
                )

            manifest = root / "targets.json"
            manifest.write_text(
                json.dumps(
                    [
                        {
                            "gn": "//build/beads/verifications/cc:beads_cc_example_bin",
                            "bazel": "//build/beads/verifications/cc:beads_cc_example_bin",
                            "bazel_config": "fuchsia_platform",
                            "type": "cpp_link",
                        },
                        {
                            "gn": "//build/beads/verifications/rust:beads_rust_example_bin.actual(//build/toolchain:host_x64)",
                            "bazel": "//build/beads/verifications/rust:beads_rust_example_bin",
                            "bazel_config": "host",
                            "type": "rustc",
                        },
                    ]
                )
            )
            build_dir = root / "out/default"

            # First config fails run_regenerator; second config should step up to
            # run the --config=host query, and both second and third configs should
            # print FAIL in their progress lines due to the host query diff.
            def fake_regen(
                fuchsia_dir: pathlib.Path,
                config_build_dir: pathlib.Path,
                verbose: bool = False,
            ) -> None:
                if config_build_dir.name.endswith("core.x64-asan"):
                    raise subprocess.CalledProcessError(1, ["regenerator.py"])

            queried_configs_by_dir: dict[str, list[str]] = {}

            def fake_verify(
                fuchsia_dir: pathlib.Path,
                build_dir: pathlib.Path,
                target_queries: list[compare_utils.CompareCommandsQuery],
                read_response_files: bool = False,
                debug_fn: object = None,
            ) -> verify_build_commands.VerificationRunResult:
                queried_configs_by_dir[build_dir.name] = [
                    q.bazel_config for q in target_queries
                ]
                cmp_results = []
                for q in target_queries:
                    if q.bazel_config == compare_utils.BAZEL_CONFIG_HOST:
                        cmp_results.append(
                            compare_utils.CompareCommandsResult(
                                query=q,
                                normalized_gn_args=["rustc", "-Copt-level=1"],
                                normalized_bazel_args=[
                                    "rustc",
                                    "-Copt-level=2",
                                ],
                            )
                        )
                    else:
                        cmp_results.append(
                            compare_utils.CompareCommandsResult(
                                query=q,
                                normalized_gn_args=["clang++", "-O1"],
                                normalized_bazel_args=["clang++", "-O1"],
                            )
                        )
                return verify_build_commands.summarize_comparison_results(
                    build_dir, cmp_results
                )

            out_buf = io.StringIO()
            err_buf = io.StringIO()
            with (
                mock.patch.object(
                    verify_multi_build_configs,
                    "run_regenerator",
                    side_effect=fake_regen,
                ),
                mock.patch.object(
                    verify_build_commands,
                    "verify_build_commands_for_build_dir",
                    side_effect=fake_verify,
                ),
                contextlib.redirect_stdout(out_buf),
                contextlib.redirect_stderr(err_buf),
            ):
                rc = verify_multi_build_configs.main(
                    [
                        "--fuchsia-dir",
                        str(root),
                        "--build-dir",
                        str(build_dir),
                        "--manifest",
                        str(manifest),
                        "--fint-params",
                        "core.x64-asan",
                        "--fint-params",
                        "core.arm64-asan",
                        "--fint-params",
                        "core.arm64-hwasan",
                        "--keep-bazel-daemons",
                        "--jobs",
                        "1",
                    ]
                )

            self.assertEqual(rc, 1)
            # core.arm64-asan ran both fuchsia_platform and host queries since core.x64-asan failed regen.
            self.assertEqual(
                queried_configs_by_dir["beads_verify_core.arm64-asan"],
                ["fuchsia_platform", "host"],
            )
            # core.arm64-hwasan only ran fuchsia_platform queries and merged host results from core.arm64-asan.
            self.assertEqual(
                queried_configs_by_dir["beads_verify_core.arm64-hwasan"],
                ["fuchsia_platform"],
            )
            stdout_text = out_buf.getvalue()
            self.assertIn(
                "[2/3] core.arm64-asan -> FAIL",
                stdout_text,
            )
            self.assertIn(
                "[3/3] core.arm64-hwasan -> FAIL",
                stdout_text,
            )
            self.assertIn("1 unexpected diffs", stdout_text)


if __name__ == "__main__":
    unittest.main()
