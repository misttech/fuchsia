# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest

from build.beads.scripts import flags_differences


def simple_flags_categorizer(flag: str) -> str:
    """A simple FlagsCategorizer that only maps arguments to Group A, Group B, and Other."""
    if flag.startswith("-A") or flag.startswith("--group-a"):
        return "Group A"
    if flag.startswith("-B") or flag.startswith("--group-b"):
        return "Group B"
    return "Other"


class FlagsDifferencesTest(unittest.TestCase):
    """Unit tests for flags_differences module."""

    def test_new_from_lists_empty(self) -> None:
        diff = flags_differences.FlagsDifferences.new_from_lists([], [])
        self.assertEqual(diff.gn_only, set())
        self.assertEqual(diff.bazel_only, set())
        self.assertEqual(diff.common, set())
        self.assertFalse(diff.has_differences)

    def test_new_from_lists_identical(self) -> None:
        diff = flags_differences.FlagsDifferences.new_from_lists(
            ["-Wall", "-O2"], ["-Wall", "-O2"]
        )
        self.assertEqual(diff.gn_only, set())
        self.assertEqual(diff.bazel_only, set())
        self.assertEqual(diff.common, {"-Wall", "-O2"})
        self.assertFalse(diff.has_differences)

    def test_new_from_lists_disjoint(self) -> None:
        diff = flags_differences.FlagsDifferences.new_from_lists(
            ["-A1", "-A2"], ["-B1", "-B2"]
        )
        self.assertEqual(diff.gn_only, {"-A1", "-A2"})
        self.assertEqual(diff.bazel_only, {"-B1", "-B2"})
        self.assertEqual(diff.common, set())
        self.assertTrue(diff.has_differences)

    def test_new_from_lists_overlapping(self) -> None:
        gn_flags = ["-Wall", "-O2", "-DFOO", "-Wall"]
        bazel_flags = ["-Wall", "-O3", "-DFOO", "-fPIC"]
        diff = flags_differences.FlagsDifferences.new_from_lists(
            gn_flags, bazel_flags
        )
        self.assertEqual(diff.gn_only, {"-O2"})
        self.assertEqual(diff.bazel_only, {"-O3", "-fPIC"})
        self.assertEqual(diff.common, {"-DFOO", "-Wall"})
        self.assertTrue(diff.has_differences)

    def test_has_differences(self) -> None:
        self.assertFalse(
            flags_differences.FlagsDifferences(common={"-Wall"}).has_differences
        )
        self.assertTrue(
            flags_differences.FlagsDifferences(gn_only={"-O2"}).has_differences
        )
        self.assertTrue(
            flags_differences.FlagsDifferences(
                bazel_only={"-O3"}
            ).has_differences
        )
        self.assertTrue(
            flags_differences.FlagsDifferences(
                gn_only={"-O2"}, bazel_only={"-O3"}
            ).has_differences
        )

    def test_default_categorizer(self) -> None:
        self.assertEqual(
            flags_differences.default_categorizer("-Wall"), "Other"
        )
        self.assertEqual(flags_differences.default_categorizer("-O2"), "Other")
        self.assertEqual(flags_differences.default_categorizer(""), "Other")

    def test_simple_flags_categorizer(self) -> None:
        self.assertEqual(simple_flags_categorizer("-A1"), "Group A")
        self.assertEqual(simple_flags_categorizer("--group-a=foo"), "Group A")
        self.assertEqual(simple_flags_categorizer("-B2"), "Group B")
        self.assertEqual(simple_flags_categorizer("--group-b=bar"), "Group B")
        self.assertEqual(simple_flags_categorizer("-C3"), "Other")
        self.assertEqual(simple_flags_categorizer("-Wall"), "Other")

    def test_get_categories_map_with_simple_categorizer(self) -> None:
        gn_flags = ["-A_gn", "-B_gn", "-Other_gn", "--common"]
        bazel_flags = ["-A_bazel", "-B_bazel", "-Other_bazel", "--common"]
        diff = flags_differences.FlagsDifferences.new_from_lists(
            gn_flags, bazel_flags
        )

        categories_map = diff.get_categories_map(simple_flags_categorizer)
        self.assertEqual(
            set(categories_map.keys()), {"Group A", "Group B", "Other"}
        )

        group_a = categories_map["Group A"]
        self.assertEqual(group_a.gn_only, {"-A_gn"})
        self.assertEqual(group_a.bazel_only, {"-A_bazel"})
        self.assertEqual(group_a.common, set())

        group_b = categories_map["Group B"]
        self.assertEqual(group_b.gn_only, {"-B_gn"})
        self.assertEqual(group_b.bazel_only, {"-B_bazel"})
        self.assertEqual(group_b.common, set())

        other = categories_map["Other"]
        self.assertEqual(other.gn_only, {"-Other_gn"})
        self.assertEqual(other.bazel_only, {"-Other_bazel"})
        self.assertEqual(other.common, set())

    def test_get_categories_map_empty(self) -> None:
        diff = flags_differences.FlagsDifferences.new_from_lists(
            ["--common"], ["--common"]
        )
        categories_map = diff.get_categories_map(simple_flags_categorizer)
        self.assertEqual(categories_map, {})

    def test_generate_summary_default_categorizer(self) -> None:
        diff = flags_differences.FlagsDifferences.new_from_lists(
            ["-Wall", "-O2"], ["-Wall", "-O3"]
        )
        summary = diff.generate_summary()
        self.maxDiff = None
        self.assertListEqual(
            summary.splitlines(),
            [
                "=" * 60,
                "FLAG COMPARISON SUMMARY",
                "=" * 60,
                "Matching flags:   1",
                "GN-only flags:    1",
                "Bazel-only flags: 1",
                "-" * 60,
                "",
                "[Other]",
                "  In GN only (missing in Bazel):",
                "    -O2",
                "  In Bazel only (extra in Bazel):",
                "    -O3",
                "",
                "=" * 60,
                "Unified diff",
                "=" * 60,
                "--- gn_flags",
                "+++ bazel_flags",
                "@@ -1,2 +1,2 @@",
                " -Wall",
                "--O2",
                "+-O3",
            ],
        )

    def test_generate_summary_with_simple_categorizer_and_custom_title(
        self,
    ) -> None:
        diff = flags_differences.FlagsDifferences.new_from_lists(
            ["-A_gn", "--common"], ["-B_bazel", "--common"]
        )
        summary = diff.generate_summary(
            title="Custom Differences",
            flag_categorizer=simple_flags_categorizer,
        )
        self.maxDiff = None
        self.assertListEqual(
            summary.splitlines(),
            [
                "=" * 60,
                "Custom Differences",
                "=" * 60,
                "Matching flags:   1",
                "GN-only flags:    1",
                "Bazel-only flags: 1",
                "-" * 60,
                "",
                "[Group A]",
                "  In GN only (missing in Bazel):",
                "    -A_gn",
                "",
                "[Group B]",
                "  In Bazel only (extra in Bazel):",
                "    -B_bazel",
                "",
                "=" * 60,
                "Unified diff",
                "=" * 60,
                "--- gn_flags",
                "+++ bazel_flags",
                "@@ -1,2 +1,2 @@",
                "--A_gn",
                "+-B_bazel",
                " --common",
            ],
        )

    def test_generate_markdown_report_default_categorizer(self) -> None:
        diff = flags_differences.FlagsDifferences.new_from_lists(
            ["-Wall", "-O2"], ["-Wall", "-O3"]
        )
        md = diff.generate_markdown_report()
        self.maxDiff = None
        self.assertListEqual(
            md.splitlines(),
            [
                "### Flag Comparison Report",
                "",
                "| Metric | Count |",
                "| :--- | :--- |",
                "| Matching Flags | 1 |",
                "| GN Only (Missing in Bazel) | 1 |",
                "| Bazel Only (Extra in Bazel) | 1 |",
                "",
                "#### Other",
                "| Status | Flag |",
                "| :--- | :--- |",
                "| GN only | `-O2` |",
                "| Bazel only | `-O3` |",
                "",
                "### Unified Diff",
                "--- gn_flags",
                "+++ bazel_flags",
                "@@ -1,2 +1,2 @@",
                " -Wall",
                "--O2",
                "+-O3",
            ],
        )

    def test_generate_markdown_report_with_simple_categorizer_and_custom_title(
        self,
    ) -> None:
        diff = flags_differences.FlagsDifferences.new_from_lists(
            ["-A_gn"], ["-B_bazel"]
        )
        md = diff.generate_markdown_report(
            title="My Markdown Title",
            flag_categorizer=simple_flags_categorizer,
        )
        self.maxDiff = None
        self.assertListEqual(
            md.splitlines(),
            [
                "### My Markdown Title",
                "",
                "| Metric | Count |",
                "| :--- | :--- |",
                "| Matching Flags | 0 |",
                "| GN Only (Missing in Bazel) | 1 |",
                "| Bazel Only (Extra in Bazel) | 1 |",
                "",
                "#### Group A",
                "| Status | Flag |",
                "| :--- | :--- |",
                "| GN only | `-A_gn` |",
                "",
                "#### Group B",
                "| Status | Flag |",
                "| :--- | :--- |",
                "| Bazel only | `-B_bazel` |",
                "",
                "### Unified Diff",
                "--- gn_flags",
                "+++ bazel_flags",
                "@@ -1 +1 @@",
                "--A_gn",
                "+-B_bazel",
            ],
        )

    def test_categorize_clang_flag(self) -> None:
        categorize_clang_flag = flags_differences.categorize_clang_flag

        # Linker Options
        self.assertEqual(
            categorize_clang_flag("-L/path/to/lib"), "Linker Options"
        )
        self.assertEqual(categorize_clang_flag("-lfoo"), "Linker Options")
        self.assertEqual(
            categorize_clang_flag("-Wl,--fatal-warnings"), "Linker Options"
        )
        self.assertEqual(
            categorize_clang_flag("-Wl,-rpath=/lib"), "Linker Options"
        )

        # Warnings (-W) - note that -Wl, is handled as Linker Options above
        self.assertEqual(categorize_clang_flag("-Wall"), "Warnings (-W)")
        self.assertEqual(categorize_clang_flag("-Wextra"), "Warnings (-W)")
        self.assertEqual(categorize_clang_flag("-Werror"), "Warnings (-W)")
        self.assertEqual(
            categorize_clang_flag("-Wno-unused-parameter"), "Warnings (-W)"
        )

        # Defines (-D)
        self.assertEqual(categorize_clang_flag("-DFOO"), "Defines (-D)")
        self.assertEqual(categorize_clang_flag("-DBAR=1"), "Defines (-D)")
        self.assertEqual(categorize_clang_flag("-D_GNU_SOURCE"), "Defines (-D)")

        # Includes (-I)
        self.assertEqual(
            categorize_clang_flag("-I/usr/include"), "Includes (-I)"
        )
        self.assertEqual(
            categorize_clang_flag("-isystem/usr/local/include"), "Includes (-I)"
        )
        self.assertEqual(
            categorize_clang_flag("-iquote/local/include"), "Includes (-I)"
        )

        # Optimization (-O)
        self.assertEqual(categorize_clang_flag("-O0"), "Optimization (-O)")
        self.assertEqual(categorize_clang_flag("-O1"), "Optimization (-O)")
        self.assertEqual(categorize_clang_flag("-O2"), "Optimization (-O)")
        self.assertEqual(categorize_clang_flag("-O3"), "Optimization (-O)")
        self.assertEqual(categorize_clang_flag("-Os"), "Optimization (-O)")
        self.assertEqual(categorize_clang_flag("-Oz"), "Optimization (-O)")

        # Prefix Mappings / Determinism - note precedence over -f
        self.assertEqual(
            categorize_clang_flag("-ffile-prefix-map=/a=/b"),
            "Prefix Mappings / Determinism",
        )
        self.assertEqual(
            categorize_clang_flag("-fdebug-prefix-map=/a=/b"),
            "Prefix Mappings / Determinism",
        )
        self.assertEqual(
            categorize_clang_flag("-fmacro-prefix-map=/a=/b"),
            "Prefix Mappings / Determinism",
        )
        self.assertEqual(
            categorize_clang_flag("-fcoverage-prefix-map=/a=/b"),
            "Prefix Mappings / Determinism",
        )
        self.assertEqual(
            categorize_clang_flag("-fprofile-prefix-map=/a=/b"),
            "Prefix Mappings / Determinism",
        )
        self.assertEqual(
            categorize_clang_flag("-ffile-compilation-dir=."),
            "Prefix Mappings / Determinism",
        )
        self.assertEqual(
            categorize_clang_flag("--remap-path-prefix=/a=/b"),
            "Prefix Mappings / Determinism",
        )

        # Compiler Features (-f)
        self.assertEqual(
            categorize_clang_flag("-fPIC"), "Compiler Features (-f)"
        )
        self.assertEqual(
            categorize_clang_flag("-fno-exceptions"), "Compiler Features (-f)"
        )
        self.assertEqual(
            categorize_clang_flag("-fvisibility=hidden"),
            "Compiler Features (-f)",
        )

        # Machine Options (-m)
        self.assertEqual(
            categorize_clang_flag("-march=x86-64"), "Machine Options (-m)"
        )
        self.assertEqual(
            categorize_clang_flag("-mcpu=cortex-a53"), "Machine Options (-m)"
        )
        self.assertEqual(categorize_clang_flag("-m64"), "Machine Options (-m)")

        # Debug Info (-g)
        self.assertEqual(categorize_clang_flag("-g"), "Debug Info (-g)")
        self.assertEqual(categorize_clang_flag("-g2"), "Debug Info (-g)")
        self.assertEqual(
            categorize_clang_flag("-gline-tables-only"), "Debug Info (-g)"
        )

        # Language Standard (-std)
        self.assertEqual(
            categorize_clang_flag("-std=c++20"), "Language Standard (-std)"
        )
        self.assertEqual(
            categorize_clang_flag("-std=c11"), "Language Standard (-std)"
        )

        # Target / Sysroot
        self.assertEqual(
            categorize_clang_flag("--target=x86_64-fuchsia"), "Target / Sysroot"
        )
        self.assertEqual(
            categorize_clang_flag("--sysroot=/path/to/sysroot"),
            "Target / Sysroot",
        )

        # Other
        self.assertEqual(categorize_clang_flag("-c"), "Other")
        self.assertEqual(categorize_clang_flag("-o"), "Other")
        self.assertEqual(categorize_clang_flag("foo.cc"), "Other")
        self.assertEqual(categorize_clang_flag("--unknown"), "Other")

    def test_categorize_rust_flag(self) -> None:
        categorize = flags_differences.categorize_rust_flag

        # Linker Options
        self.assertEqual(categorize("-Clinker=clang++"), "Linker Options")
        self.assertEqual(
            categorize("-Clink-arg=-Wl,-rpath=/lib"), "Linker Options"
        )
        self.assertEqual(categorize("-Ldependency=out/lib"), "Linker Options")
        self.assertEqual(categorize("-lstatic=foo"), "Linker Options")

        # Optimization
        self.assertEqual(categorize("-Copt-level=3"), "Optimization (-O)")
        self.assertEqual(categorize("-O"), "Optimization (-O)")

        # Debug Info
        self.assertEqual(categorize("-Cdebuginfo=2"), "Debug Info (-g)")
        self.assertEqual(
            categorize("-Cdebug-assertions=yes"), "Debug Info (-g)"
        )
        self.assertEqual(categorize("-g"), "Debug Info (-g)")

        # Prefix Mappings / Determinism
        self.assertEqual(
            categorize("--remap-path-prefix=/a=/b"),
            "Prefix Mappings / Determinism",
        )
        self.assertEqual(
            categorize("-Cmetadata=12345"), "Prefix Mappings / Determinism"
        )
        self.assertEqual(
            categorize("-Cextra-filename=-abc"), "Prefix Mappings / Determinism"
        )

        # Lints / Warnings
        self.assertEqual(categorize("-Aunused_variables"), "Lints / Warnings")
        self.assertEqual(categorize("--allow=dead_code"), "Lints / Warnings")
        self.assertEqual(categorize("-Wdeprecated"), "Lints / Warnings")
        self.assertEqual(categorize("-Dwarnings"), "Lints / Warnings")
        self.assertEqual(
            categorize("-Funconditional_recursion"), "Lints / Warnings"
        )
        self.assertEqual(categorize("--cap-lints=allow"), "Lints / Warnings")

        # Conditional Compilation
        self.assertEqual(
            categorize('--cfg=feature="std"'),
            "Conditional Compilation (--cfg)",
        )
        self.assertEqual(
            categorize('--cfg=__rust_toolchain="custom"'),
            "Conditional Compilation (--cfg)",
        )
        self.assertEqual(
            categorize("--check-cfg=cfg()"),
            "Conditional Compilation (--cfg)",
        )

        # Dependencies
        self.assertEqual(
            categorize("--extern=foo=libfoo.rlib"), "Dependencies (--extern)"
        )

        # Codegen Options (-C)
        self.assertEqual(
            categorize("-Cembed-bitcode=no"), "Codegen Options (-C)"
        )
        self.assertEqual(
            categorize("-Ccodegen-units=16"), "Codegen Options (-C)"
        )
        self.assertEqual(
            categorize("-Cstrip=debuginfo"), "Codegen Options (-C)"
        )
        self.assertEqual(categorize("-Cpanic=abort"), "Codegen Options (-C)")

        # Language Edition
        self.assertEqual(
            categorize("--edition=2024"), "Language Edition (--edition)"
        )

        # Target / Sysroot
        self.assertEqual(
            categorize("--target=x86_64-fuchsia"), "Target / Sysroot"
        )
        self.assertEqual(
            categorize("--sysroot=path/to/sysroot"), "Target / Sysroot"
        )

        # Crate Outputs & Formats
        self.assertEqual(
            categorize("--crate-type=rlib"), "Crate Outputs & Formats"
        )
        self.assertEqual(
            categorize("--crate-name=my_crate"), "Crate Outputs & Formats"
        )
        self.assertEqual(
            categorize("--emit=dep-info,link"), "Crate Outputs & Formats"
        )
        self.assertEqual(
            categorize("--out-dir=out/bin"), "Crate Outputs & Formats"
        )
        self.assertEqual(
            categorize("-o=out/bin/my_bin"), "Crate Outputs & Formats"
        )
        self.assertEqual(
            categorize("--error-format=human"), "Crate Outputs & Formats"
        )

        # Unstable Options (-Z)
        self.assertEqual(
            categorize("-Zshell-argfiles"), "Unstable Options (-Z)"
        )
        self.assertEqual(
            categorize("-Zdep-info-omit-d-target"), "Unstable Options (-Z)"
        )

        # Other
        self.assertEqual(categorize("rustc"), "Other")
        self.assertEqual(categorize("src/lib.rs"), "Other")
        self.assertEqual(categorize("RUST_BACKTRACE=1"), "Other")
        self.assertEqual(categorize("@shell:args.txt"), "Other")


if __name__ == "__main__":
    unittest.main()
