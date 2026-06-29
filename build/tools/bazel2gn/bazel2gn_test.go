// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn_test

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"
	"go.fuchsia.dev/fuchsia/build/tools/bazel2gn"
	"go.starlark.net/syntax"
)

// toSyntaxFile is a test helper that parses the input string (content of a
// BUILD.bazel file) to a *syntax.File.
func toSyntaxFile(t *testing.T, s string) *syntax.File {
	t.Helper()

	p := filepath.Join(t.TempDir(), "BUILD.bazel.test")
	if err := os.WriteFile(p, []byte(s), 0600); err != nil {
		t.Fatalf("Failed to write test Bazel file: %v", err)
	}

	f, err := bazel2gn.Parse(p)
	if err != nil {
		t.Fatalf("Failed to parse test Bazel build file: %v, file content:\n%s", err, s)
	}
	return f
}

// bazelToGN is a test helper that converts all statements in a *syntax.File to
// content of a BUILD.gn.
func bazelToGN(f *syntax.File) (string, error) {
	var gotLines []string
	for _, stmt := range f.Stmts {
		lines, err := bazel2gn.StmtToGN(stmt)
		if err != nil {
			return "", fmt.Errorf("converting Bazel statement to GN: %v", err)
		}
		gotLines = append(gotLines, lines...)
	}
	return strings.Join(gotLines, "\n"), nil
}

func TestStmtToGN(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "Simple Go targets",
			bazel: `load("@io_bazel_rules_go//go:def.bzl", "go_binary", "go_library", "go_test")

go_library(
	name = "bazel2gn",
	srcs = [
		"bazel2gn.go",
	],
	deps = [
		"//third_party/golibs:go.starlark.net/syntax",
	],
)

go_binary(
	name = "cmd",
	srcs = [
		"cmd/main.go",
	],
	deps = [
		":bazel2gn",
		"//third_party/golibs:go.starlark.net/starlark",
		"//third_party/golibs:go.starlark.net/syntax",
	],
)

go_test(
	name = "bazel2gn_tests",
	embed = [ ":bazel2gn" ],
	srcs = [
		"bazel2gn_test.go",
	],
	deps = [
		"//third_party/golibs:github.com/google/go-cmp/cmp",
		"//third_party/golibs:go.starlark.net/starlark",
		"//third_party/golibs:go.starlark.net/syntax",
	],
)`,
			wantGN: `go_library("bazel2gn") {
	sources = [
		"bazel2gn.go",
	]
	deps = [
		"//third_party/golibs:go.starlark.net/syntax",
	]
}
go_binary("cmd") {
	sources = [
		"cmd/main.go",
	]
	deps = [
		":bazel2gn",
		"//third_party/golibs:go.starlark.net/starlark",
		"//third_party/golibs:go.starlark.net/syntax",
	]
}
go_test("bazel2gn_tests") {
	embed = [
		":bazel2gn",
	]
	sources = [
		"bazel2gn_test.go",
	]
	deps = [
		"//third_party/golibs:github.com/google/go-cmp/cmp",
		"//third_party/golibs:go.starlark.net/starlark",
		"//third_party/golibs:go.starlark.net/syntax",
	]
}`,
		},
		{
			name: "Test suite",
			bazel: `test_suite(
	name = "tests",
	tests = [
		"//tools/check-licenses/directory:directory_test",
		"//tools/check-licenses/file:file_test",
	],
)`,
			wantGN: `group("tests") {
	deps = [
		"//tools/check-licenses/directory:directory_test",
		"//tools/check-licenses/file:file_test",
	]
	testonly = true
}`,
		},
		{
			name: "Stamp group",
			bazel: `stamp_group(
	name = "tests",
	deps = [
		"//tools/check-licenses/directory:directory_test",
		"//tools/check-licenses/file:file_test",
	],
)`,
			wantGN: `group("tests") {
	deps = [
		"//tools/check-licenses/directory:directory_test",
		"//tools/check-licenses/file:file_test",
	]
}`,
		},
		{
			name: "Simple Python targets",
			bazel: `load("@rules_python//python:defs.bzl", "py_binary", "py_library")

py_library(
	name = "generate_version_history",
	srcs = ["__init__.py"],
	imports = [".."],
)

py_binary(
	name = "generate_version_history_bin",
	srcs = ["cmd.py"],
	main = "cmd.py",
	deps = [":generate_version_history"],
)`,
			wantGN: `python_library("generate_version_history") {
	sources = [
		"__init__.py",
	]
}
python_binary("generate_version_history_bin") {
	sources = [
		"cmd.py",
	]
	main_source = "cmd.py"
	deps = [
		":generate_version_history",
	]
}`,
		},
		{
			name: "Empty list attributes",
			bazel: `fx_cc_library(
	name = "has_empty_lists",
	# 'configs' has special handling so test both it and another attribute.
	configs = [],
	srcs = [],
)
`,
			wantGN: `static_library("has_empty_lists") {
	configs += [
	]
	sources = [
	]
}`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			gotGN, err := bazelToGN(f)
			if err != nil {
				t.Fatalf("Unexpected failure converting Bazel build targets: %v", err)
			}
			if diff := cmp.Diff(gotGN, tc.wantGN); diff != "" {
				t.Errorf("Diff found after GN conversion (-got +want):\n%s\nBazel source:\n%s", diff, tc.bazel)
			}
		})
	}
}

func TestDictConversion(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "zither_fidl_library with zither",
			bazel: `load("//build/bazel/rules/fidl:fidl_library.bzl", "zither_fidl_library")

zither_fidl_library(
    name = "zbi",
    srcs = [
        "board.fidl",
        "cpu.fidl",
        "driver-config.fidl",
        "graphics.fidl",
        "kernel.fidl",
        "memory.fidl",
        "overview.fidl",
        "partition.fidl",
        "reboot.fidl",
        "secure-entropy.fidl",
        "zbi.fidl",
    ],
    enable_zither = True,
    experimental_flags = ["zx_c_types"],
    visibility = ["//visibility:public"],
    zither = {
        "c": {
            # The C backend is used to generate checked-in headers within this
            # include namespace.
            "output_namespace": "lib/zbi-format",
        },
    },
)
`,
			wantGN: `fidl("zbi") {
	sources = [
		"board.fidl",
		"cpu.fidl",
		"driver-config.fidl",
		"graphics.fidl",
		"kernel.fidl",
		"memory.fidl",
		"overview.fidl",
		"partition.fidl",
		"reboot.fidl",
		"secure-entropy.fidl",
		"zbi.fidl",
	]
	enable_zither = true
	experimental_flags = [
		"zx_c_types",
	]
	visibility = [
		"*",
	]
	zither = {
		c = {
			output_namespace = "lib/zbi-format"
		}
	}
}`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			gotGN, err := bazelToGN(f)
			if err != nil {
				t.Fatalf("Unexpected failure converting Bazel build targets: %v", err)
			}
			if diff := cmp.Diff(gotGN, tc.wantGN); diff != "" {
				t.Errorf("Diff found after GN conversion (-got +want):\n%s\nBazel source:\n%s", diff, tc.bazel)
			}
		})
	}
}

func TestTargetCompatibleWith(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "HOST_CONSTRAINTS",
			bazel: `
load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")

go_binary(
	name = "host_tool",
	srcs = [
		"main.go",
	],
	target_compatible_with = HOST_CONSTRAINTS,
)`,
			wantGN: `if (is_host) {
	go_binary("host_tool") {
		sources = [
			"main.go",
		]
	}
}`,
		},
		{
			name: "HOST_OS_CONSTRAINTS",
			bazel: `
load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")

go_binary(
	name = "host_tool",
	srcs = [
		"main.go",
	],
	target_compatible_with = HOST_OS_CONSTRAINTS,
)`,
			wantGN: `if (is_host) {
	go_binary("host_tool") {
		sources = [
			"main.go",
		]
	}
}`,
		},
		{
			// Due to the current limited options in `bazelConstraintsToGNConditions`,
			// the Fuchsia condition is duplicated to exercise the list logic.
			name: "list of constraints",
			bazel: `
go_binary(
	name = "constrained_tool",
	srcs = [
		"main.go",
	],
	target_compatible_with = [
		"@platforms//os:fuchsia",
		"@platforms//os:fuchsia",
	],
)`,
			wantGN: `if (is_fuchsia && is_fuchsia) {
	go_binary("constrained_tool") {
		sources = [
			"main.go",
		]
	}
}`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			gotGN, err := bazelToGN(f)
			if err != nil {
				t.Fatalf("Unexpected failure converting Bazel build targets: %v", err)
			}
			if diff := cmp.Diff(gotGN, tc.wantGN); diff != "" {
				t.Errorf("Diff found after GN conversion (-got +want):\n%s\nBazel source:\n%s", diff, tc.bazel)
			}
		})
	}
}

func TestTargetCompatibleWithErrors(t *testing.T) {
	for _, tc := range []struct {
		name  string
		bazel string
	}{
		{
			name: "unexpected target_compatible_with variable",
			bazel: `
go_binary(
	name = "host_tool",
	srcs = [
		"main.go",
	],
	target_compatible_with = UNSUPPORTED_CONSTRAINTS,
)`,
		},
		{
			name: "list of constraints not supported yet",
			bazel: `
go_binary(
	name = "host_tool",
	srcs = [
		"main.go",
	],
	target_compatible_with = [
		"@platforms//os:linux",
		"@platforms//cpu:x86_64",
	],
)`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			_, err := bazelToGN(f)
			if err == nil {
				t.Fatal("Expecting failure converting Bazel targets, got nil")
			}
		})
	}
}

func TestFileLevelConstants(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "simple",
			bazel: `zbi_sources = [
		"board.fidl",
		"cpu.fidl",
]

fidl_library(
    name = "zbi",
    srcs = zbi_sources,
)
`,
			wantGN: `zbi_sources = [
	"board.fidl",
	"cpu.fidl",
]

# To avoid "Assignment had no effect" from GN.
# It's possible this variable is only used in if conditions (e.g. is_host).
not_needed([ "zbi_sources" ])

fidl("zbi") {
	sources = zbi_sources
}`,
		},
		{
			name: "is_host guards",
			bazel: `load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")

_COMMON_SOURCES = [
	"foo.go",
	"bar.go",
]

go_library(
	name = "lib1",
	srcs = _COMMON_SOURCES,
	target_compatible_with = HOST_CONSTRAINTS,
)

go_library(
	name = "lib2",
	srcs = _COMMON_SOURCES,
	target_compatible_with = HOST_CONSTRAINTS,
)`,
			wantGN: `_COMMON_SOURCES = [
	"foo.go",
	"bar.go",
]

# To avoid "Assignment had no effect" from GN.
# It's possible this variable is only used in if conditions (e.g. is_host).
not_needed([ "_COMMON_SOURCES" ])

if (is_host) {
	go_library("lib1") {
		sources = _COMMON_SOURCES
	}
}
if (is_host) {
	go_library("lib2") {
		sources = _COMMON_SOURCES
	}
}`,
		},
		{
			name: "file_level_visibility variable assignment",
			bazel: `_foo_visibility = [
				"//bar:__pkg__",
				"//baz:__subpackages__",
			]`,
			wantGN: `_foo_visibility = [
	"//bar:*",
	"//baz/*",
]

# To avoid "Assignment had no effect" from GN.
# It's possible this variable is only used in if conditions (e.g. is_host).
not_needed([ "_foo_visibility" ])
`,
		},
		{
			name: "top-level deps assignment rust crate rewriting",
			bazel: `
# @bazel2gn:transformer=deps
FOO_DEPS = [
	"//third_party/rust_crates/vendor:lock_api",
]`,
			wantGN: `FOO_DEPS = [
	"//third_party/rust_crates:lock_api",
]

# To avoid "Assignment had no effect" from GN.
# It's possible this variable is only used in if conditions (e.g. is_host).
not_needed([ "FOO_DEPS" ])
`,
		}, {
			name: "target transformer annotation rust crate rewriting",
			bazel: `
rustc_library(
	name="herp",
	# @bazel2gn:transformer=deps
	derps = [
		"//third_party/rust_crates/vendor:lock_api",
	],
)`,
			wantGN: `rustc_library("herp") {
	derps = [
		"//third_party/rust_crates:lock_api",
	]
}`,
		}, {
			name: "target transformer annotation suffix rust crate rewriting",
			bazel: `
rustc_library(
	name="herp",
	derps = ["//third_party/rust_crates/vendor:lock_api"], # @bazel2gn:transformer=deps
)`,
			wantGN: `rustc_library("herp") {
	derps = [
		"//third_party/rust_crates:lock_api",
	]
}`,
		},
	} {
		f := toSyntaxFile(t, tc.bazel)
		gotGN, err := bazelToGN(f)
		if err != nil {
			t.Fatalf("Unexpected failure converting Bazel build targets: %v", err)
		}
		if diff := cmp.Diff(gotGN, tc.wantGN); diff != "" {
			t.Errorf("Diff found after GN conversion (-got +want):\n%s\nBazel source:\n%s", diff, tc.bazel)
		}
	}
}

func TestIDKConversion(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "IDK C++ source library",
			bazel: `load("//build/bazel/rules/idk:idk_cc_source_library.bzl", "idk_cc_source_library")

idk_cc_source_library(
	name = "foo",
	api_area = "Media",
	category = "partner",
	idk_name = "foobar",
	stable = True,
	hdrs = ["include/lib/foobar/foobar_defs.h"],
	hdrs_for_internal_use = ["path/to/internal.h"],
	public_configs = [":foo_include"],
	deps = ["//path/to/public_deps"],
	implementation_deps = ["//path/to/implementation_deps"],
	visibility = [ "//visibility:public" ],
)
`,
			wantGN: `sdk_source_set("foo") {
	sdk_area = "Media"
	category = "partner"
	sdk_name = "foobar"
	stable = true
	public = [
		"include/lib/foobar/foobar_defs.h",
	]
	sdk_headers_for_internal_use = [
		"path/to/internal.h",
	]
	public += [
		"path/to/internal.h",
	]
	public_configs = [
		":foo_include",
	]
	public_deps = [
		"//path/to/public_deps",
	]
	deps = [
		"//path/to/implementation_deps",
	]
	visibility = [
		"*",
	]
}`,
		},
		{
			name: "IDK C++ source library for Zircon library",
			// This test case should be identical to the one for
			// `idk_cc_source_library()` except for `sdk_publishable` in the
			// expectation and `sdk` the input and expectation.
			bazel: `load("//build/bazel/rules/idk:idk_cc_source_library.bzl", "idk_cc_source_library_zx")

idk_cc_source_library_zx(
	name = "foo",
	api_area = "Media",
	category = "partner",
	idk_name = "foobar",
	stable = True,
	hdrs = ["include/lib/foobar/foobar_defs.h"],
	hdrs_for_internal_use = ["path/to/internal.h"],
	public_configs = [":foo_include"],
	deps = ["//path/to/public_deps"],
	implementation_deps = ["//path/to/implementation_deps"],
	visibility = [ "//visibility:public" ],
)
`,
			wantGN: `zx_library("foo") {
	sdk_area = "Media"
	sdk_publishable = "partner"
	sdk_name = "foobar"
	stable = true
	public = [
		"include/lib/foobar/foobar_defs.h",
	]
	sdk_headers_for_internal_use = [
		"path/to/internal.h",
	]
	public += [
		"path/to/internal.h",
	]
	public_configs = [
		":foo_include",
	]
	public_deps = [
		"//path/to/public_deps",
	]
	deps = [
		"//path/to/implementation_deps",
	]
	visibility = [
		"*",
	]
	sdk = "source"
}`,
		},
		{
			name: "fuchsia_deps",
			bazel: `idk_cc_source_library(
	name = "foo",
	public_deps = ["//sdk/lib/stdcompat"],
	fuchsia_deps = [
		"//zircon/system/ulib/zx",
	],
	non_fuchsia_deps = [
		"//zircon/system/ulib/zx_host",
	],
	fuchsia_implementation_deps = [
		"//zircon/system/ulib/zx_impl",
	],
)
`,
			wantGN: `sdk_source_set("foo") {
	public_deps = [
		"//sdk/lib/stdcompat",
	]
	if (is_fuchsia) {
		public_deps += [
			"//zircon/system/ulib/zx",
		]
	}
	if (!is_fuchsia) {
		public_deps += [
			"//zircon/system/ulib/zx_host",
		]
	}
	if (is_fuchsia) {
		deps += [
			"//zircon/system/ulib/zx_impl",
		]
	}
}`,
		},
		{
			name: "Fuchsia and non-Fuchsia source files",
			bazel: `load("//build/bazel/rules/idk:idk_cc_source_library.bzl", "idk_cc_source_library")

list_of_files = ["baz.cc"]
list_of_internal_hdrs = ["include/lib/foobar/internal/internal_baz.h"]

idk_cc_source_library(
	name = "foo",
	api_area = "Developer",
	category = "partner",
	idk_name = "foobar",
	stable = True,
	srcs = ["source.cc"] + list_of_files + select({
		"@platforms//os:fuchsia": ["source_fuchsia.cc"],
		"//conditions:default": ["source_host.cc"],
	}),
	hdrs = ["include/lib/foobar/foobar.h"] + select({
		"@platforms//os:fuchsia": ["include/lib/foobar/foobar_fuchsia.h"],
		"//conditions:default": [],
	}),
	hdrs_for_internal_use = ["include/lib/foobar/internal/internal.h"] + list_of_internal_hdrs + select({
		"@platforms//os:fuchsia": [
			"include/lib/foobar/internal/internal_fuchsia.h",
			"include/lib/foobar/internal/internal_fuchsia_helper.h",
		],
		"//conditions:default": [
			"include/lib/foobar/internal/internal_host.h"
		],
	}),
)
`,
			wantGN: `list_of_files = [
	"baz.cc",
]

# To avoid "Assignment had no effect" from GN.
# It's possible this variable is only used in if conditions (e.g. is_host).
not_needed([ "list_of_files" ])

list_of_internal_hdrs = [
	"include/lib/foobar/internal/internal_baz.h",
]

# To avoid "Assignment had no effect" from GN.
# It's possible this variable is only used in if conditions (e.g. is_host).
not_needed([ "list_of_internal_hdrs" ])

sdk_source_set("foo") {
	sdk_area = "Developer"
	category = "partner"
	sdk_name = "foobar"
	stable = true
	sources = []
	sources += [
		"source.cc",
	]
	sources += list_of_files
	if (is_fuchsia) {
		sources += [
			"source_fuchsia.cc",
		]
	} else {
		sources += [
			"source_host.cc",
		]
	}
	public = []
	public += [
		"include/lib/foobar/foobar.h",
	]
	if (is_fuchsia) {
		public += [
			"include/lib/foobar/foobar_fuchsia.h",
		]
	}
	sdk_headers_for_internal_use = []
	sdk_headers_for_internal_use += [
		"include/lib/foobar/internal/internal.h",
	]
	public += [
		"include/lib/foobar/internal/internal.h",
	]
	sdk_headers_for_internal_use += list_of_internal_hdrs
	public += list_of_internal_hdrs
	if (is_fuchsia) {
		sdk_headers_for_internal_use += [
			"include/lib/foobar/internal/internal_fuchsia.h",
			"include/lib/foobar/internal/internal_fuchsia_helper.h",
		]
		public += [
			"include/lib/foobar/internal/internal_fuchsia.h",
			"include/lib/foobar/internal/internal_fuchsia_helper.h",
		]
	} else {
		sdk_headers_for_internal_use += [
			"include/lib/foobar/internal/internal_host.h",
		]
		public += [
			"include/lib/foobar/internal/internal_host.h",
		]
	}
}`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			gotGN, err := bazelToGN(f)
			if err != nil {
				t.Fatalf("Unexpected failure converting Bazel build targets: %v", err)
			}
			if diff := cmp.Diff(gotGN, tc.wantGN); diff != "" {
				t.Errorf("Diff found after GN conversion (-got +want):\n%s\nBazel source:\n%s", diff, tc.bazel)
			}
		})
	}
}

func TestCCConversion(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "Simple C++ targets",
			bazel: `cc_library(
	name = "foo",
	srcs = [
	"path/to/bar.cc",
	"path/to/bar.h",
	"path/to/baz.cc",
		"path/to/foo.cc",
		"yet/another/path/to/foo.cc",
	],
	hdrs = [
		"path/to/baz.h",
		"path/to/foo.h",
	],
	deps = [
		"//path/to:foo",
		"//yet/another/path/to:bar",
	],
	implementation_deps = [
		"//path/to:bar",
	],
	copts = [
		"-Wno-implicit-fallthrough",
	],
	visibility = [
		":__pkg__",
		"//path/to/dir:__subpackages__",
	],
)
`,
			wantGN: `static_library("foo") {
	sources = [
		"path/to/bar.cc",
		"path/to/bar.h",
		"path/to/baz.cc",
		"path/to/foo.cc",
		"yet/another/path/to/foo.cc",
	]
	public = [
		"path/to/baz.h",
		"path/to/foo.h",
	]
	public_deps = [
		"//path/to:foo",
		"//yet/another/path/to:bar",
	]
	deps = [
		"//path/to:bar",
	]
	configs += [
		"//build/config:Wno-implicit-fallthrough",
	]
	visibility = [
		":*",
		"//path/to/dir/*",
	]
}`,
		},
		{
			name: "ldflags with raw_overwrite",
			bazel: `cc_library(
	name = "fdio",
	ldflags = [
		"-Wl,--version-script=sdk/lib/fdio/fdio.ld", # @bazel2gn:raw_overwrite:"-Wl,--version-script=" + rebase_path("fdio.ld", root_build_dir)
	],
)
`,
			wantGN: `static_library("fdio") {
	ldflags = [
		"-Wl,--version-script=" + rebase_path("fdio.ld", root_build_dir),
	]
}`,
		},
		{
			name: "select in copts to configs",
			bazel: `cc_library(
	name = "foo",
	copts = select({
		"@platforms//os:fuchsia": [ "-Wno-implicit-fallthrough" ],
		"//conditions:default": [],
	}),
)
`,
			wantGN: `static_library("foo") {
	if (is_fuchsia) {
		configs += [
			"//build/config:Wno-implicit-fallthrough",
		]
	}
}`,
		},
		{
			name: "configs append by default",
			bazel: `cc_library(
	name = "configs_append",
	copts = [],
)
`,
			wantGN: `static_library("configs_append") {
	configs += [
	]
}`,
		},
		{
			name: "explicit config clearing with annotation",
			bazel: `cc_library(
	name = "empty_configs",
	copts = [], # @bazel2gn:clear
)
`,
			wantGN: `static_library("empty_configs") {
	configs = [
	]
}`,
		},
		{
			name: "irrelevant comments are ignored",
			bazel: `cc_library(
	name = "empty_configs",
	copts = [], # this comment does NOT affect bazel2gn
)
`,
			wantGN: `static_library("empty_configs") {
	configs += [
	]
}`,
		},
		{
			name: "comments above are ignored",
			bazel: `cc_library(
	name = "comment_above",
	# @bazel2gn:clear
	copts = [],
)
`,
			wantGN: `static_library("comment_above") {
	configs += [
	]
}`,
		},
		{
			name: "ignore fuchsia_api_level_copts",
			bazel: `cc_library(
	name = "foo",
	copts = [
		"-Wno-vla-cxx-extension",
	] + fuchsia_api_level_copts(),
)
`,
			wantGN: `static_library("foo") {
	configs += [
		"//build/config:Wno-vla-cxx-extension",
	]
}`,
		},
		{
			name: "ignore only fuchsia_api_level_copts",
			bazel: `cc_library(
	name = "foo",
	copts = fuchsia_api_level_copts(),
)
`,
			wantGN: `static_library("foo") {
}`,
		},
		{
			name: "alwayslink = True converts to source_set",
			bazel: `cc_library(
	name = "foo",
	alwayslink = True,
)
`,
			wantGN: `source_set("foo") {
}`,
		},
		{
			name: "alwayslink = False converts to static_library",
			bazel: `cc_library(
	name = "foo",
	alwayslink = False,
)
`,
			wantGN: `static_library("foo") {
}`,
		},
		{
			name: "fx_cc_library alwayslink = True converts to source_set",
			bazel: `fx_cc_library(
	name = "foo",
	alwayslink = True,
)
`,
			wantGN: `source_set("foo") {
}`,
		},
		{
			name: "fx_cc_library alwayslink = False converts to static_library",
			bazel: `fx_cc_library(
	name = "foo",
	alwayslink = False,
)
`,
			wantGN: `static_library("foo") {
}`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			gotGN, err := bazelToGN(f)
			if err != nil {
				t.Fatalf("Unexpected failure converting Bazel build targets: %v", err)
			}
			if diff := cmp.Diff(gotGN, tc.wantGN); diff != "" {
				t.Errorf("Diff found after GN conversion (-got +want):\n%s\nBazel source:\n%s", diff, tc.bazel)
			}
		})
	}
}

func TestZxConversion(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "extra gn expression",
			bazel: `load("//build/bazel/rules:zx_library.bzl", "cc_source_library_zx")

cc_source_library_zx(
    name = "cmdline",
    srcs = ["args_parser.cc"],
    hdrs = [
        "include/lib/cmdline/args_parser.h",
        "include/lib/cmdline/optional.h",
        "include/lib/cmdline/status.h",
    ],
    includes = ["include"],
    visibility = ["//visibility:public"],
)
`,
			wantGN: `zx_library("cmdline") {
	sources = [
		"args_parser.cc",
	]
	public = [
		"include/lib/cmdline/args_parser.h",
		"include/lib/cmdline/optional.h",
		"include/lib/cmdline/status.h",
	]
	includes = [
		"include",
	]
	visibility = [
		"*",
	]
	sdk = "source"
}`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			gotGN, err := bazelToGN(f)
			if err != nil {
				t.Fatalf("Unexpected failure converting Bazel build targets: %v", err)
			}
			if diff := cmp.Diff(gotGN, tc.wantGN); diff != "" {
				t.Errorf("Diff found after GN conversion (-got +want):\n%s\nBazel source:\n%s", diff, tc.bazel)
			}
		})
	}
}

func TestSkipAnnotationError(t *testing.T) {
	for _, tc := range []struct {
		name  string
		bazel string
	}{
		{
			name: "skip annotation not immediately before target",
			bazel: `
# @bazel2gn:skip
# Some other comment
go_library(
    name = "foo",
)
`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			_, err := bazelToGN(f)
			if err == nil {
				t.Errorf("Expected error, but got nil, Bazel source:\n%s", tc.bazel)
			}
		})
	}
}

func TestSkipAnnotation(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "Skip attributes",
			bazel: `
go_library(
    name = "foo",
    srcs = ["foo.cc"],
    # @bazel2gn:skip
    pure = "off",
    deps = ["//foo/bar"],
    cgo = "on", # @bazel2gn:skip
)
`,
			wantGN: `go_library("foo") {
	sources = [
		"foo.cc",
	]
	deps = [
		"//foo/bar",
	]
}`,
		},
		{
			name: "Skip annotations",
			bazel: `
# @bazel2gn:skip
go_library(
    name = "foo",
)

# @bazel2gn:skip

go_library(
    name = "bar",
)

# Some other comment
# @bazel2gn:skip
go_library(
    name = "baz",
)
`,
			wantGN: ``,
		},
		{
			name: "NOT skip annotations",
			bazel: `
# @bazel2gn:skip-please
go_library(
    name = "qux",
)
`,
			wantGN: `go_library("qux") {
}`,
		},
		{
			name: "Multiple targets mixed",
			bazel: `
go_library(
    name = "foo",
)

# @bazel2gn:skip
go_test(
    name = "bar",
)

go_binary(
    name = "baz",
)
`,
			wantGN: `go_library("foo") {
}
go_binary("baz") {
}`,
		},
		{
			name: "Skip list member with comment above",
			bazel: `
cc_library(
	name = "foo",
	copt = [
		# @bazel2gn:skip
		"-ffuchsia-api-level=4293918720",
		"-Wno-vla-cxx-extension",
	],
)
`,
			wantGN: `static_library("foo") {
	copt = [
		"-Wno-vla-cxx-extension",
	]
}`,
		},
		{
			name: "Skip list member with comment inline",
			bazel: `
cc_library(
	name = "foo",
	copt = [
		"-ffuchsia-api-level=4293918720", # @bazel2gn:skip
		"-Wno-vla-cxx-extension",
	],
)
`,
			wantGN: `static_library("foo") {
	copt = [
		"-Wno-vla-cxx-extension",
	]
}`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			gotGN, err := bazelToGN(f)
			if err != nil {
				t.Fatalf("Unexpected failure converting Bazel build targets: %v", err)
			}
			if diff := cmp.Diff(gotGN, tc.wantGN); diff != "" {
				t.Errorf("Diff found after GN conversion (-got +want):\n%s\nBazel source:\n%s", diff, tc.bazel)
			}
		})
	}
}
