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
	importpath = "go.fuchsia.dev/fuchsia/build/tools/bazel2gn",
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
	importpath = "go.fuchsia.dev/fuchsia/build/tools/bazel2gn"
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
			name: "success",
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
	bazel := `zbi_sources = [
	"board.fidl",
	"cpu.fidl",
]

fidl_library(
    name = "zbi",
    srcs = zbi_sources,
)
`
	wantGN := `zbi_sources = [
	"board.fidl",
	"cpu.fidl",
]
fidl("zbi") {
	sources = zbi_sources
}`
	f := toSyntaxFile(t, bazel)
	gotGN, err := bazelToGN(f)
	if err != nil {
		t.Fatalf("Unexpected failure converting Bazel build targets: %v", err)
	}
	if diff := cmp.Diff(gotGN, wantGN); diff != "" {
		t.Errorf("Diff found after GN conversion (-got +want):\n%s\nBazel source:\n%s", diff, bazel)
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
			bazel: `load("//build/bazel/bazel_idk:defs.bzl", "idk_cc_source_library")

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
	public += sdk_headers_for_internal_use
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
			bazel: `load("//build/bazel/bazel_idk:defs.bzl", "idk_cc_source_library_zx")

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
	public += sdk_headers_for_internal_use
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
			wantGN: `source_set("foo") {
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
			name: "select in copts to configs",
			bazel: `cc_library(
	name = "foo",
	copts = select({
		"@platforms//os:fuchsia": [ "-Wno-implicit-fallthrough" ],
		"//conditions:default": [],
	}),
)
`,
			wantGN: `source_set("foo") {
	if (is_fuchsia) {
		configs += [
			"//build/config:Wno-implicit-fallthrough",
		]
	} else {
		configs += [
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
			wantGN: `source_set("configs_append") {
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
			wantGN: `source_set("empty_configs") {
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
			wantGN: `source_set("empty_configs") {
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
			wantGN: `source_set("comment_above") {
	configs += [
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

func TestHasSkipAnnotation(t *testing.T) {
	for _, tc := range []struct {
		name  string
		bazel string
		want  []bool
	}{
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
			want: []bool{true, true, true},
		},
		{
			name: "NOT skip annotations",
			bazel: `
# @bazel2gn:skip-please
go_library(
    name = "qux",
)
`,
			want: []bool{false},
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
			want: []bool{false, true, false},
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			var got []bool

			for _, stmt := range f.Stmts {
				hasSkip, err := bazel2gn.HasSkipAnnotation(stmt)
				if err != nil {
					t.Fatalf("Unexpected error in HasSkipAnnotation: %v", err)
				}
				got = append(got, hasSkip)
			}

			if diff := cmp.Diff(got, tc.want); diff != "" {
				t.Errorf("Diff found in HasSkipAnnotation results (-got +want):\n%s\nBazel source:\n%s", diff, tc.bazel)
			}
		})
	}
}

func TestHasSkipAnnotationError(t *testing.T) {
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
		{
			name: "skip annotation after target",
			bazel: `
go_library(
    name = "baz",
) # @bazel2gn:skip
`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			if len(f.Stmts) != 1 {
				t.Fatalf("Unexpected number of statements in Bazel source: %v, want 1", len(f.Stmts))
			}
			_, err := bazel2gn.HasSkipAnnotation(f.Stmts[0])
			if err == nil {
				t.Errorf("Expected error, but got nil, Bazel source:\n%s", tc.bazel)
			}
		})
	}
}
