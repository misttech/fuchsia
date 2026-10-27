#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Tests for trim_visibility.py.

To run:
$ fx test //build/bazel/scripts:trim_visibility_test
"""

import textwrap
import unittest
from typing import Sequence

from trim_visibility import (
    PRIVATE,
    PUBLIC,
    Entry,
    Kind,
    Label,
    PackageGroup,
    Status,
    analyze_list,
    build_file_package,
    covers,
    entry_covers_path,
    entry_to_bazel,
    entry_to_gn,
    expected_gn_entries,
    format_bazel2gn_overwrite,
    format_bazel_list,
    gn_list_to_bazel,
    gn_target_is_generated_by_bazel2gn,
    label_reference_regex,
    package_group_contains,
    parse_bazel_build_output,
    parse_bazel_entry,
    parse_gn_entry,
    parse_gn_instances,
    parse_label,
    parse_labels,
    parse_macro_locations,
    parse_ninja_depfile_inputs,
    parse_package_group_build_output,
    parse_source_visibility,
    sorted_labels,
    subsumes,
)

OWN = "foo/lib"


def pkg(package: str) -> Entry:
    return Entry(Kind.PACKAGE, package)


def subpkgs(package: str) -> Entry:
    return Entry(Kind.SUBPACKAGES, package)


def target(label: str) -> Entry:
    parsed = parse_label(label)
    return Entry(Kind.TARGET, parsed.package, parsed.name)


class LabelTest(unittest.TestCase):
    def test_parse_label(self) -> None:
        self.assertEqual(parse_label("//a/b:c"), Label("a/b", "c"))
        self.assertEqual(parse_label("//a/b"), Label("a/b", "b"))
        self.assertEqual(parse_label("@//a/b:c"), Label("a/b", "c"))
        self.assertEqual(parse_label("@@//a/b:c"), Label("a/b", "c"))
        self.assertEqual(
            parse_label("//a/b:c(//build/toolchain:host_x64)"),
            Label("a/b", "c"),
        )
        self.assertEqual(parse_label(":c", "a/b"), Label("a/b", "c"))
        self.assertEqual(parse_label("//:c"), Label("", "c"))
        self.assertEqual(parse_label("//.:c"), Label("", "c"))
        self.assertEqual(str(Label("a/b", "c")), "//a/b:c")

    def test_parse_label_errors(self) -> None:
        for text in ("@rules_cc//cc:defs", "a/b:c", "//a/b:", ":c"):
            with self.assertRaises(ValueError, msg=text):
                parse_label(text)

    def test_parse_labels(self) -> None:
        self.assertEqual(
            parse_labels(
                [
                    "//a:b",
                    "//a:b(//build/toolchain:host_x64)",
                    "@@rules_cc+//cc:foo",
                    "INFO: something",
                    "  //c/d:e  ",
                ]
            ),
            {Label("a", "b"), Label("c/d", "e")},
        )


class EntryTest(unittest.TestCase):
    def test_parse_bazel_entry(self) -> None:
        self.assertEqual(parse_bazel_entry("//visibility:public", OWN), PUBLIC)
        self.assertEqual(
            parse_bazel_entry("//visibility:private", OWN), PRIVATE
        )
        self.assertEqual(parse_bazel_entry("//a/b:__pkg__", OWN), pkg("a/b"))
        self.assertEqual(
            parse_bazel_entry("//a/b:__subpackages__", OWN), subpkgs("a/b")
        )
        self.assertEqual(parse_bazel_entry(":__pkg__", OWN), pkg(OWN))
        self.assertEqual(
            parse_bazel_entry("//:__subpackages__", OWN), subpkgs("")
        )
        self.assertEqual(
            parse_bazel_entry("//a/b:group", OWN),
            Entry(Kind.PACKAGE_GROUP, "a/b", "group"),
        )

    def test_parse_gn_entry(self) -> None:
        self.assertEqual(parse_gn_entry("*", OWN), PUBLIC)
        self.assertEqual(parse_gn_entry("//*", OWN), subpkgs(""))
        self.assertEqual(parse_gn_entry("//a/b/*", OWN), subpkgs("a/b"))
        self.assertEqual(parse_gn_entry("//a/b:*", OWN), pkg("a/b"))
        self.assertEqual(parse_gn_entry("//:*", OWN), pkg(""))
        self.assertEqual(parse_gn_entry("//.:*", OWN), pkg(""))
        self.assertEqual(parse_gn_entry(":*", OWN), pkg(OWN))
        self.assertEqual(parse_gn_entry("./*", OWN), subpkgs(OWN))
        self.assertEqual(parse_gn_entry("//a/b:c", OWN), target("//a/b:c"))
        self.assertEqual(parse_gn_entry("//a/b", OWN), target("//a/b:b"))
        self.assertEqual(
            parse_gn_entry("//a/b:*(//build/toolchain:host_x64)", OWN),
            pkg("a/b"),
        )

    def test_bazel_round_trip(self) -> None:
        for text in (
            "//visibility:public",
            "//visibility:private",
            "//a/b:__pkg__",
            "//a/b:__subpackages__",
            "//:__subpackages__",
            "//a/b:group",
        ):
            self.assertEqual(
                entry_to_bazel(parse_bazel_entry(text, OWN), OWN), text
            )

    def test_gn_round_trip(self) -> None:
        for text in ("*", "//*", "//a/b/*", "//a/b:*", "//a/b:c", ":*", "./*"):
            self.assertEqual(entry_to_gn(parse_gn_entry(text, OWN), OWN), text)

    def test_bazel_to_gn_matches_bazel2gn(self) -> None:
        # See bazelVisibilityToGN in //build/tools/bazel2gn/transformers.go.
        cases = {
            "//visibility:public": "*",
            "//visibility:private": ":*",
            ":__subpackages__": "./*",
            "//:__subpackages__": "//*",
            "//a/b:__pkg__": "//a/b:*",
            "//a/b:__subpackages__": "//a/b/*",
            "//a/b:group": "//a/b:group",
        }
        for bazel, gn in cases.items():
            self.assertEqual(
                entry_to_gn(parse_bazel_entry(bazel, OWN), OWN), gn, bazel
            )

    def test_gn_to_bazel(self) -> None:
        self.assertIsNone(entry_to_bazel(pkg(OWN), OWN))
        self.assertIsNone(entry_to_bazel(target(f"//{OWN}:x"), OWN))
        self.assertEqual(
            entry_to_bazel(target("//a/b:c"), OWN), "//a/b:__pkg__"
        )
        self.assertEqual(
            gn_list_to_bazel(
                [
                    subpkgs("a"),
                    pkg("a/b"),
                    target("//c:d"),
                    target("//c:e"),
                    pkg(OWN),
                ],
                OWN,
            ),
            ["//a:__subpackages__", "//c:__pkg__"],
        )

    def test_covers(self) -> None:
        consumer = Label("a/b", "c")
        self.assertTrue(covers(PUBLIC, consumer, {}))
        self.assertFalse(covers(PRIVATE, consumer, {}))
        self.assertTrue(covers(pkg("a/b"), consumer, {}))
        self.assertFalse(covers(pkg("a"), consumer, {}))
        self.assertFalse(covers(pkg("a/b/c"), consumer, {}))
        self.assertTrue(covers(subpkgs("a"), consumer, {}))
        self.assertTrue(covers(subpkgs("a/b"), consumer, {}))
        self.assertTrue(covers(subpkgs(""), consumer, {}))
        self.assertFalse(covers(subpkgs("a/bc"), Label("a/b", "c"), {}))
        self.assertFalse(covers(subpkgs("a/b"), Label("a/bc", "c"), {}))
        self.assertTrue(covers(target("//a/b:c"), consumer, {}))
        self.assertFalse(covers(target("//a/b:d"), consumer, {}))
        group = Entry(Kind.PACKAGE_GROUP, "g", "group")
        self.assertIsNone(covers(group, consumer, {}))

    def test_subsumes(self) -> None:
        self.assertTrue(subsumes(PUBLIC, pkg("a")))
        self.assertTrue(subsumes(subpkgs("a"), pkg("a/b")))
        self.assertTrue(subsumes(subpkgs("a"), subpkgs("a/b")))
        self.assertTrue(subsumes(subpkgs("a"), pkg("a")))
        self.assertTrue(subsumes(pkg("a"), target("//a:b")))
        self.assertTrue(subsumes(pkg("a"), pkg("a")))
        self.assertFalse(subsumes(pkg("a"), pkg("a/b")))
        self.assertFalse(subsumes(subpkgs("a/b"), subpkgs("a")))
        self.assertFalse(subsumes(subpkgs("a"), pkg("ab")))
        self.assertFalse(subsumes(pkg("a"), PRIVATE))
        self.assertFalse(
            subsumes(subpkgs(""), Entry(Kind.PACKAGE_GROUP, "a", "g"))
        )


class PackageGroupTest(unittest.TestCase):
    def test_contains(self) -> None:
        groups = {
            Label("g", "a"): PackageGroup(
                packages=["//foo/...", "-//foo/private", "//bar"],
                includes=[Label("g", "b")],
            ),
            Label("g", "b"): PackageGroup(packages=["//baz"], includes=[]),
            Label("g", "c"): PackageGroup(
                packages=[], includes=[Label("g", "missing")]
            ),
        }
        a = Label("g", "a")
        self.assertTrue(package_group_contains(a, "foo", groups))
        self.assertTrue(package_group_contains(a, "foo/x", groups))
        self.assertFalse(package_group_contains(a, "foo/private", groups))
        self.assertTrue(package_group_contains(a, "bar", groups))
        self.assertFalse(package_group_contains(a, "bar/x", groups))
        self.assertTrue(package_group_contains(a, "baz", groups))
        self.assertFalse(package_group_contains(a, "qux", groups))
        self.assertIsNone(package_group_contains(Label("g", "c"), "x", groups))
        self.assertIsNone(package_group_contains(Label("g", "x"), "x", groups))

    def test_public(self) -> None:
        groups = {Label("g", "p"): PackageGroup(["public"], [])}
        self.assertTrue(package_group_contains(Label("g", "p"), "x", groups))


class ParseOutputTest(unittest.TestCase):
    def test_parse_bazel_build_output(self) -> None:
        output = textwrap.dedent(
            """\
            # /out/workspace/sdk/lib/vfs/cpp/BUILD.bazel:9:22
            cc_library(
              name = "cpp",
              visibility = ["//sdk:__pkg__", "//sdk/lib/vfs/cpp:__pkg__"],
              generator_name = "cpp",
              generator_function = "idk_cc_source_library",
              deps = ["//zircon/system/ulib/zx:zx"],
            )
            # Rule cpp instantiated at (most recent call last):
            """
        )
        info = parse_bazel_build_output(output)
        assert info is not None
        self.assertEqual(info.kind, "cc_library")
        self.assertEqual(
            info.visibility, ["//sdk:__pkg__", "//sdk/lib/vfs/cpp:__pkg__"]
        )
        self.assertEqual(info.generator_name, "cpp")

    def test_parse_bazel_build_output_no_visibility(self) -> None:
        info = parse_bazel_build_output('alias(\n  name = "a",\n)\n')
        assert info is not None
        self.assertIsNone(info.visibility)
        self.assertIsNone(info.generator_name)

    def test_parse_package_group(self) -> None:
        output = textwrap.dedent(
            """\
            # /out/workspace/src/testing/fidl/BUILD.bazel:35:14
            package_group(
              name = "fidl_macro_visibility",
              includes = ["//other:group"],
              packages = ["//build/bazel/rules/fidl", "-//x/..."],
            )
            """
        )
        group = parse_package_group_build_output(output)
        assert group is not None
        self.assertEqual(
            group.packages, ["//build/bazel/rules/fidl", "-//x/..."]
        )
        self.assertEqual(group.includes, [Label("other", "group")])
        self.assertIsNone(
            parse_package_group_build_output('cc_library(\n  name = "a",\n)\n')
        )

    def test_parse_macro_locations(self) -> None:
        ws = "/out/gen/build/bazel/workspace"
        output = textwrap.dedent(
            f"""\
            # {ws}/sdk/fidl/fuchsia.ui.views/BUILD.bazel:9:13
            cc_library(
              name = "fuchsia.ui.views_hlcpp",
              visibility = ["//visibility:public"],
            )
            # Rule fuchsia.ui.views_hlcpp instantiated at (most recent call last):
            #   {ws}/sdk/fidl/fuchsia.ui.views/BUILD.bazel:9:13        in <toplevel>
            #   {ws}/build/bazel/rules/fidl/fidl_library.bzl:647:18    in fidl_library
            #   {ws}/build/bazel/rules/fidl/fidl_cc_library.bzl:399:19 in _fidl_cc_library
            #   /out/output_base/external/rules_cc+/cc/cc_library.bzl:19:16 in cc_library
            # Rule cc_library defined at (most recent call last):
            #   /virtual_builtins_bzl/common/cc/cc_library.bzl:565:18 in <toplevel>
            # {ws}/src/foo/BUILD.bazel:3:11
            cc_binary(
              name = "bin",
            )
            # Rule bin instantiated at (most recent call last):
            #   {ws}/src/foo/BUILD.bazel:3:11 in <toplevel>
            # Rule cc_binary defined at (most recent call last):
            #   {ws}/build/bazel/rules/cc/defs.bzl:1:1 in <toplevel>
            """
        )
        views = Label("sdk/fidl/fuchsia.ui.views", "fuchsia.ui.views_hlcpp")
        self.assertEqual(
            parse_macro_locations(output, [views, Label("src/foo", "bin")]),
            {views: ["build/bazel/rules/fidl"]},
        )

    def test_parse_macro_locations_suffix_packages(self) -> None:
        ws = "/out/gen/build/bazel/workspace"
        output = textwrap.dedent(
            f"""\
            # {ws}/sdk/lib/vfs/cpp/BUILD.bazel:9:13
            cc_library(
              name = "lib",
            )
            # Rule lib instantiated at (most recent call last):
            #   {ws}/sdk/lib/vfs/cpp/BUILD.bazel:9:13 in <toplevel>
            #   {ws}/build/bazel/rules/cc/macro.bzl:10:5 in my_macro
            # Rule cc_library defined at (most recent call last):
            """
        )
        short = Label("cpp", "lib")
        specific = Label("sdk/lib/vfs/cpp", "lib")
        self.assertEqual(
            parse_macro_locations(output, [short, specific]),
            {specific: ["build/bazel/rules/cc"]},
        )

    def test_parse_gn_instances(self) -> None:
        output = textwrap.dedent(
            """\
            //foo/lib:lib
            //foo/lib:lib_unittests
            //foo/lib:lib(//build/toolchain:host_x64)
            //foo/lib:other(//build/toolchain:host_x64)
            """
        )
        self.assertEqual(
            parse_gn_instances(output, Label("foo/lib", "lib")),
            ["//foo/lib:lib", "//foo/lib:lib(//build/toolchain:host_x64)"],
        )

    def test_parse_ninja_depfile_inputs(self) -> None:
        self.assertEqual(
            parse_ninja_depfile_inputs(
                "build.ninja.stamp: ../../BUILD.gn ../../a/BUILD.gn \\\n"
                " ../../b/c.gni\n"
            ),
            ["../../BUILD.gn", "../../a/BUILD.gn", "../../b/c.gni"],
        )

    def test_gn_target_is_generated_by_bazel2gn(self) -> None:
        build_gn = textwrap.dedent(
            """\
            group("tests") {
            }

            ## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##

            if (is_fuchsia) {
              sdk_source_set("cpp") {
              }
            }
            """
        )
        self.assertTrue(gn_target_is_generated_by_bazel2gn(build_gn, "cpp"))
        self.assertFalse(gn_target_is_generated_by_bazel2gn(build_gn, "tests"))
        self.assertFalse(
            gn_target_is_generated_by_bazel2gn('group("cpp") {}', "cpp")
        )


class SourceVisibilityTest(unittest.TestCase):
    BUILD = textwrap.dedent(
        """\
        load("//build/bazel/rules:foo.bzl", "foo_library")

        package(default_applicable_licenses = ["//:license"])

        # @bazel2gn:transformer=visibility
        shared_visibility = [
            "//a:__pkg__",
            "//b:__subpackages__",
        ]

        foo_library(
            name = "plain",
            visibility = [
                "//c:__pkg__",
                ":__subpackages__",
            ],
        )

        foo_library(
            name = "split",
            visibility = [
                "//d:__pkg__",
            ],  # @bazel2gn:raw_overwrite:[ "//*" ]
        )

        foo_library(
            name = "empty_split",
            visibility = [],  # @bazel2gn:raw_overwrite:[ "//e/*" ]
        )

        foo_library(
            name = "element_split",
            visibility = [
                "//visibility:private",  # @bazel2gn:raw_overwrite:":*", "//f:*"
            ],
        )

        foo_library(
            name = "skip_annotation",
            visibility = ["//a:__pkg__"],  # @bazel2gn:skip
        )

        foo_library(
            name = "variable",
            visibility = shared_visibility + ["//g:__pkg__"],
        )

        foo_library(
            name = "dynamic",
            visibility = some_function(),
        )

        foo_library(
            name = "default",
        )
        """
    )

    def parse(self, name: str) -> tuple[bool, list[str] | None, list[str]]:
        result = parse_source_visibility(self.BUILD, name)
        assert result is not None
        return result.present, result.entries, result.overwrites

    def test_plain(self) -> None:
        self.assertEqual(
            self.parse("plain"), (True, ["//c:__pkg__", ":__subpackages__"], [])
        )

    def test_split(self) -> None:
        self.assertEqual(
            self.parse("split"),
            (True, ["//d:__pkg__"], ['raw_overwrite:[ "//*" ]']),
        )
        self.assertEqual(
            self.parse("empty_split"),
            (True, [], ['raw_overwrite:[ "//e/*" ]']),
        )
        self.assertEqual(
            self.parse("element_split"),
            (True, ["//visibility:private"], ['raw_overwrite:":*", "//f:*"']),
        )
        self.assertEqual(
            self.parse("skip_annotation"),
            (True, ["//a:__pkg__"], ["skip"]),
        )

    def test_variable(self) -> None:
        self.assertEqual(
            self.parse("variable"),
            (True, ["//a:__pkg__", "//b:__subpackages__", "//g:__pkg__"], []),
        )

    def test_unresolvable(self) -> None:
        self.assertEqual(self.parse("dynamic"), (True, None, []))

    def test_absent(self) -> None:
        self.assertEqual(self.parse("default"), (False, None, []))

    def test_missing_target(self) -> None:
        self.assertIsNone(parse_source_visibility(self.BUILD, "missing"))
        self.assertIsNone(parse_source_visibility("foo(", "missing"))


class TextSearchTest(unittest.TestCase):
    def test_label_reference_regex(self) -> None:
        self.assertEqual(
            label_reference_regex(Label("zircon/system/ulib/svc", "svc")),
            '"//zircon/system/ulib/svc(:svc)?["(]',
        )
        self.assertEqual(
            label_reference_regex(Label("sdk/lib/vfs/cpp", "cpp")),
            '"//sdk/lib/vfs/cpp(:cpp)?["(]',
        )
        self.assertEqual(
            label_reference_regex(Label("sdk/lib/c++", "lib.x")),
            '"//sdk/lib/c\\+\\+:lib\\.x["(]',
        )

    def test_entry_covers_path(self) -> None:
        self.assertTrue(entry_covers_path(pkg("a/b"), "a/b/BUILD.gn", {}))
        self.assertFalse(entry_covers_path(pkg("a/b"), "a/b/c/BUILD.gn", {}))
        self.assertTrue(entry_covers_path(subpkgs("a/b"), "a/b/c/BUILD.gn", {}))
        self.assertTrue(
            entry_covers_path(subpkgs("a/b"), "a/b/BUILD.bazel", {})
        )
        self.assertFalse(entry_covers_path(subpkgs("a/b"), "a/bc/BUILD.gn", {}))
        self.assertTrue(entry_covers_path(target("//a/b:c"), "a/b/x.gni", {}))
        self.assertTrue(entry_covers_path(PUBLIC, "a/BUILD.gn", {}))
        self.assertFalse(entry_covers_path(PRIVATE, "a/BUILD.gn", {}))
        group = Entry(Kind.PACKAGE_GROUP, "g", "group")
        self.assertIsNone(entry_covers_path(group, "a/BUILD.gn", {}))

    def test_entry_covers_secondary_source(self) -> None:
        self.assertEqual(
            build_file_package("build/secondary/third_party/ogg/BUILD.gn"),
            "third_party/ogg",
        )
        self.assertEqual(
            build_file_package("build/secondary/foo.gni"), "build/secondary"
        )
        self.assertTrue(
            entry_covers_path(
                subpkgs("third_party/ogg"),
                "build/secondary/third_party/ogg/BUILD.gn",
                {},
            )
        )
        self.assertFalse(
            entry_covers_path(
                subpkgs("build/secondary"),
                "build/secondary/third_party/ogg/BUILD.gn",
                {},
            )
        )


class AnalyzeTest(unittest.TestCase):
    def statuses(self, report_entries: Sequence[object]) -> dict[str, Status]:
        return {e.text: e.status for e in report_entries}  # type: ignore[attr-defined]

    def test_bazel_list(self) -> None:
        report = analyze_list(
            [
                "//a:__pkg__",
                "//b:__subpackages__",
                "//b/c:__pkg__",
                "//d:__pkg__",
                "//e:__pkg__",
                f"//{OWN}:__pkg__",
                "//a:__pkg__",
            ],
            parse_bazel_entry,
            OWN,
            consumers=[
                Label("a", "x"),
                Label("b/c", "y"),
                Label("z", "uncovered"),
                Label(OWN, "internal"),
                Label("sdk", "atom"),
            ],
            implicit_entries=[pkg(OWN), pkg("sdk")],
            reference_files=[
                "a/BUILD.gn",
                "e/BUILD.gn",
                "sdk/BUILD.bazel",
                "y/BUILD.gn",
            ],
        )
        self.assertEqual(
            [(e.text, e.status) for e in report.entries],
            [
                ("//a:__pkg__", Status.NEEDED),
                ("//b:__subpackages__", Status.NEEDED),
                ("//b/c:__pkg__", Status.REDUNDANT),
                ("//d:__pkg__", Status.UNUSED),
                ("//e:__pkg__", Status.UNVERIFIED),
                (f"//{OWN}:__pkg__", Status.REDUNDANT),
                ("//a:__pkg__", Status.REDUNDANT),
            ],
        )
        self.assertEqual(report.entries[2].subsumed_by, "//b:__subpackages__")
        self.assertEqual(report.entries[4].text_references, ["e/BUILD.gn"])
        self.assertEqual(report.uncovered, [Label("z", "uncovered")])
        self.assertEqual(report.uncovered_references, ["y/BUILD.gn"])
        self.assertEqual(
            [e.text for e in report.kept()],
            ["//a:__pkg__", "//b:__subpackages__", "//e:__pkg__"],
        )

    def test_gn_list_own_package_is_explicit(self) -> None:
        report = analyze_list(
            [":*", "//a/*", "//b:c"],
            parse_gn_entry,
            OWN,
            consumers=[Label(OWN, "x"), Label("a/b", "y"), Label("b", "d")],
        )
        self.assertEqual(
            self.statuses(report.entries),
            {
                ":*": Status.NEEDED,
                "//a/*": Status.NEEDED,
                "//b:c": Status.UNUSED,
            },
        )
        self.assertEqual(report.uncovered, [Label("b", "d")])

    def test_package_groups(self) -> None:
        groups = {Label("g", "known"): PackageGroup(["//a/..."], [])}
        report = analyze_list(
            ["//g:known", "//g:unknown", "//g:unused_unknown"],
            parse_bazel_entry,
            OWN,
            consumers=[Label("a/b", "x"), Label("c", "y")],
            groups=groups,
        )
        self.assertEqual(
            self.statuses(report.entries),
            {
                "//g:known": Status.NEEDED,
                "//g:unknown": Status.UNKNOWN,
                "//g:unused_unknown": Status.UNKNOWN,
            },
        )
        self.assertEqual(report.undetermined, [Label("c", "y")])
        self.assertEqual(report.uncovered, [])

    def test_macro_locations(self) -> None:
        macro_consumer = Label("sdk/fidl/x", "x_hlcpp")
        report = analyze_list(
            ["//build/bazel/rules/fidl:__subpackages__", "//sdk/lib:__pkg__"],
            parse_bazel_entry,
            OWN,
            consumers=[macro_consumer, Label("sdk/lib", "y"), Label("z", "z")],
            macro_locations={
                macro_consumer: ["build/bazel/rules/fidl"],
                Label("sdk/lib", "y"): ["build/bazel/rules/fidl"],
            },
        )
        self.assertEqual(report.entries[0].consumers, [macro_consumer])
        # The consumer's own package takes precedence.
        self.assertEqual(report.entries[1].consumers, [Label("sdk/lib", "y")])
        self.assertEqual(report.uncovered, [Label("z", "z")])

    def test_check_unused(self) -> None:
        report = analyze_list(
            [
                "//a:__pkg__",
                "//vendor:__subpackages__",
                "//vendor/x:__pkg__",
                "//b:__pkg__",
            ],
            parse_bazel_entry,
            OWN,
            consumers=[],
            check_unused=lambda e: (
                (Status.UNKNOWN, "not in checkout")
                if e.package.startswith("vendor")
                else (Status.UNVERIFIED, "not loaded")
                if e.package == "b"
                else None
            ),
        )
        self.assertEqual(
            [(e.status, e.reason) for e in report.entries],
            [
                (Status.UNUSED, None),
                (Status.UNKNOWN, "not in checkout"),
                (Status.REDUNDANT, "not in checkout"),
                (Status.UNVERIFIED, "not loaded"),
            ],
        )
        self.assertEqual(
            report.entries[2].subsumed_by, "//vendor:__subpackages__"
        )

    def test_public(self) -> None:
        report = analyze_list(
            ["//visibility:public"], parse_bazel_entry, OWN, [Label("a", "b")]
        )
        self.assertEqual(report.entries[0].status, Status.NEEDED)

    def test_private(self) -> None:
        report = analyze_list(
            ["//visibility:private"], parse_bazel_entry, OWN, []
        )
        self.assertEqual(report.entries[0].status, Status.UNKNOWN)


class FormatTest(unittest.TestCase):
    def test_format_bazel_list(self) -> None:
        self.assertEqual(
            format_bazel_list(["//b:__pkg__", "//a:__subpackages__"]),
            'visibility = [\n    "//a:__subpackages__",\n    "//b:__pkg__",\n],',
        )
        self.assertEqual(
            format_bazel_list([]), 'visibility = ["//visibility:private"],'
        )

    def test_sorted_labels(self) -> None:
        self.assertEqual(
            sorted_labels(["//a/b:__pkg__", "//a:__pkg__", "//a/b/c:__pkg__"]),
            ["//a:__pkg__", "//a/b:__pkg__", "//a/b/c:__pkg__"],
        )
        self.assertEqual(
            sorted_labels(
                ["//a/b/testing:__pkg__", "//a/b:__pkg__", "//a/b:__pkg__"]
            ),
            ["//a/b:__pkg__", "//a/b/testing:__pkg__"],
        )

    def test_format_bazel2gn_overwrite(self) -> None:
        self.assertEqual(
            format_bazel2gn_overwrite(["//b:*", "//a/*"]),
            '# @bazel2gn:raw_overwrite:[ "//a/*", "//b:*" ]',
        )

    def test_expected_gn_entries(self) -> None:
        self.assertEqual(
            expected_gn_entries([pkg("a"), PRIVATE], OWN), {pkg("a"), pkg(OWN)}
        )


if __name__ == "__main__":
    unittest.main()
