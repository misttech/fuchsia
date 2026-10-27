// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn_test

import (
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"
)

func TestTagsConversion(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "sync assert_no_deps",
			bazel: `rustc_library(
    name = "lib",
    srcs = ["src/lib.rs"],
    tags = ["assert_no_deps=//third_party/rust_crates/vendor:anyhow"],
)
`,
			wantGN: `rustc_library("lib") {
	sources = [
		"src/lib.rs",
	]
	assert_no_deps = [
		"//third_party/rust_crates:anyhow",
	]
}`,
		},
		{
			name: "sync multiple assert_no_deps and ignore other tags",
			bazel: `rustc_library(
    name = "lib",
    srcs = ["src/lib.rs"],
    tags = [
        "manual",
        "assert_no_deps=//third_party/rust_crates/vendor:anyhow",
        "noclippy",
        "assert_no_deps=//src/lib/foo:bar",
    ],
)
`,
			wantGN: `rustc_library("lib") {
	sources = [
		"src/lib.rs",
	]
	assert_no_deps = [
		"//third_party/rust_crates:anyhow",
		"//src/lib/foo:bar",
	]
}`,
		},
		{
			name: "ignore tags if no assert_no_deps",
			bazel: `rustc_library(
    name = "lib",
    srcs = ["src/lib.rs"],
    tags = [
        "manual",
        "noclippy",
    ],
)
`,
			wantGN: `rustc_library("lib") {
	sources = [
		"src/lib.rs",
	]
}`,
		},
		{
			name: "sync assert_no_deps with __pkg__ and __subpackages__",
			bazel: `rustc_library(
    name = "lib",
    srcs = ["src/lib.rs"],
    tags = [
        "assert_no_deps=//src/lib/foo:__pkg__",
        "assert_no_deps=//src/lib/bar:__subpackages__",
    ],
)
`,
			wantGN: `rustc_library("lib") {
	sources = [
		"src/lib.rs",
	]
	assert_no_deps = [
		"//src/lib/foo:*",
		"//src/lib/bar/*",
	]
}`,
		},
		{
			name: "sync assert_no_deps with local __pkg__ and __subpackages__",
			bazel: `rustc_library(
    name = "lib",
    srcs = ["src/lib.rs"],
    tags = [
        "assert_no_deps=:__pkg__",
        "assert_no_deps=:__subpackages__",
    ],
)
`,
			wantGN: `rustc_library("lib") {
	sources = [
		"src/lib.rs",
	]
	assert_no_deps = [
		":*",
		"./*",
	]
}`,
		},
		{
			name: "sync assert_no_deps with path_overwrite comment",
			bazel: `rustc_library(
    name = "lib",
    srcs = ["src/lib.rs"],
    tags = [
        "assert_no_deps=//third_party/rust_crates/vendor:serde_json5",  # @bazel2gn:path_overwrite://third_party/rust_crates:serde_json5($current_toolchain)
    ],
)
`,
			wantGN: `rustc_library("lib") {
	sources = [
		"src/lib.rs",
	]
	assert_no_deps = [
		"//third_party/rust_crates:serde_json5($current_toolchain)",
	]
}`,
		},
		{
			name: "sync exclude_toolchain_tags",
			bazel: `rustc_binary(
    name = "bin",
    srcs = ["src/main.rs"],
    tags = [
        "exclude_toolchain_tags=heapdump",
        "assert_no_deps=//src/lib/foo:bar",
        "exclude_toolchain_tags=asan",
    ],
)
`,
			wantGN: `rustc_binary("bin") {
	sources = [
		"src/main.rs",
	]
	assert_no_deps = [
		"//src/lib/foo:bar",
	]
	exclude_toolchain_tags = [
		"heapdump",
		"asan",
	]
}`,
		},
		{
			name: "sync exclude_toolchain_tags on rustc_test",
			bazel: `rustc_test(
    name = "foo_test",
    srcs = ["src/lib.rs"],
    tags = ["exclude_toolchain_tags=heapdump"],
)
`,
			wantGN: `rustc_test("foo_test") {
	sources = [
		"src/lib.rs",
	]
	exclude_toolchain_tags = [
		"heapdump",
	]
}`,
		},
		{
			name: "sync exclude_toolchain_tags on rustc_library with with_unit_tests",
			bazel: `rustc_library(
    name = "lib",
    srcs = ["src/lib.rs"],
    with_unit_tests = True,
    tags = ["exclude_toolchain_tags=heapdump"],
)
`,
			wantGN: `rustc_library("lib") {
	sources = [
		"src/lib.rs",
	]
	with_unit_tests = true
	exclude_toolchain_tags = [
		"heapdump",
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

func TestTagsConversionErrors(t *testing.T) {
	for _, tc := range []struct {
		name    string
		bazel   string
		wantErr string
	}{
		{
			name: "tags not list literal",
			bazel: `rustc_library(
    name = "lib",
    srcs = ["src/lib.rs"],
    tags = select({
        "//conditions:default": ["assert_no_deps=//third_party/rust_crates/vendor:anyhow"],
    }),
)`,
			wantErr: "expected list literal for tags attribute, got *syntax.CallExpr",
		},
		{
			name: "exclude_toolchain_tags on a C++ rule",
			bazel: `fx_cc_binary(
    name = "bin",
    srcs = ["main.cc"],
    tags = ["exclude_toolchain_tags=instrumented"],
)`,
			wantErr: `the "exclude_toolchain_tags=instrumented" tag is only supported on Rust binary, library, and test rules, got fx_cc_binary`,
		},
		{
			name: "empty exclude_toolchain_tags",
			bazel: `rustc_binary(
    name = "bin",
    srcs = ["src/main.rs"],
    tags = ["exclude_toolchain_tags="],
)`,
			wantErr: `empty "exclude_toolchain_tags=" tag`,
		},
		{
			name: "exclude_toolchain_tags on rustc_library without with_unit_tests",
			bazel: `rustc_library(
    name = "lib",
    srcs = ["src/lib.rs"],
    tags = ["exclude_toolchain_tags=heapdump"],
)`,
			wantErr: `the "exclude_toolchain_tags=heapdump" tag is only supported on rustc_library when with_unit_tests is set`,
		},
		{
			name: "exclude_toolchain_tags on rustc_library with with_unit_tests=False",
			bazel: `rustc_library(
    name = "lib",
    srcs = ["src/lib.rs"],
    with_unit_tests = False,
    tags = ["exclude_toolchain_tags=heapdump"],
)`,
			wantErr: `the "exclude_toolchain_tags=heapdump" tag is only supported on rustc_library when with_unit_tests is set`,
		},
		{
			name: "exclude_toolchain_tags on rust_library without with_unit_tests",
			bazel: `rust_library(
    name = "lib",
    srcs = ["src/lib.rs"],
    tags = ["exclude_toolchain_tags=heapdump"],
)`,
			wantErr: `the "exclude_toolchain_tags=heapdump" tag is only supported on rust_library when with_unit_tests is set`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			_, err := bazelToGN(f)
			if err == nil {
				t.Fatalf("Unexpected success converting Bazel targets. Bazel source:\n%s", tc.bazel)
			} else if tc.wantErr != "" && !strings.Contains(err.Error(), tc.wantErr) {
				t.Errorf("bazelToGN() error = %q, want substring %q", err.Error(), tc.wantErr)
			}
		})
	}
}
