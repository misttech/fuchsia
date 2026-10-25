// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn_test

import (
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"
	"go.fuchsia.dev/fuchsia/build/tools/bazel2gn"
)

func TestRustTestCrate(t *testing.T) {
	bazel := `rustc_library(
	name = "foo",
	srcs = ["src/lib.rs"],
	edition = "2021",
	test_deps = [":unused"],
	visibility = ["//visibility:public"],
	with_unit_tests = True,
	deps = [":bar"],
)

rustc_test(
	name = "foo_lib_test",
	crate = ":foo",
	edition = "2024",
	build_flags = [":flags"],
	deps = [":baz"],
)`
	wantGN := `rustc_library("foo") {
	sources = [
		"src/lib.rs",
	]
	edition = "2021"
	test_deps = [
		":unused",
	]
	visibility = [
		"*",
	]
	with_unit_tests = true
	deps = [
		":bar",
	]
}
rustc_test("foo_lib_test") {
	sources = [
		"src/lib.rs",
	]
	edition = "2021"
	deps = [
		":bar",
	] + [
		":baz",
	]
	configs += [
		":flags",
	]
}`
	f := toSyntaxFile(t, bazel)
	lines, err := bazel2gn.FileToGN(f)
	if err != nil {
		t.Fatalf("FileToGN() failed: %s", err)
	}
	if diff := cmp.Diff(strings.Join(lines, "\n"), wantGN); diff != "" {
		t.Errorf("FileToGN() diff (-got +want):\n%s", diff)
	}
}

// Verifies that when a `rustc_test` sets `crate = ":bin"` on a `rustc_binary`
// whose `srcs` is `["src/main.rs"]` (with no explicit `crate_root`), the
// generated GN `rustc_test` inherits `sources = ["src/main.rs"]` and explicitly
// sets `source_root = "src/main.rs"` instead of falling back to GN
// `rustc_test`'s `src/lib.rs` default.
func TestRustTestBinaryCrate(t *testing.T) {
	bazel := `rustc_binary(
	name = "bin",
	srcs = ["src/main.rs"],
	crate_name = "foo",
	edition = "2024",
	deps = [":bar"],
)

rustc_test(
	name = "foo_bin_test",
	crate = ":bin",
	edition = "2024",
	deps = [":baz"],
)`
	wantGN := `rustc_binary("bin") {
	sources = [
		"src/main.rs",
	]
	output_name = "foo"
	edition = "2024"
	deps = [
		":bar",
	]
}
rustc_test("foo_bin_test") {
	sources = [
		"src/main.rs",
	]
	edition = "2024"
	deps = [
		":bar",
	] + [
		":baz",
	]
	source_root = "src/main.rs"
}`
	f := toSyntaxFile(t, bazel)
	lines, err := bazel2gn.FileToGN(f)
	if err != nil {
		t.Fatalf("FileToGN() failed: %s", err)
	}
	if diff := cmp.Diff(strings.Join(lines, "\n"), wantGN); diff != "" {
		t.Errorf("FileToGN() diff (-got +want):\n%s", diff)
	}
}

func TestRustTestCrateSkippedAttrs(t *testing.T) {
	bazel := `# @bazel2gn:skip
rustc_library(
	name = "foo",
	srcs = ["src/lib.rs"],
	# @bazel2gn:skip
	proc_macro_deps = [":skipped_macro"],
	deps = [":bar"],
)

rustc_test(
	name = "foo_test",
	crate = ":foo",
	# @bazel2gn:skip
	deps = [":skipped_test_dep"],
)`
	wantGN := `rustc_test("foo_test") {
	sources = [
		"src/lib.rs",
	]
	deps = [
		":bar",
	]
}`
	f := toSyntaxFile(t, bazel)
	lines, err := bazel2gn.FileToGN(f)
	if err != nil {
		t.Fatalf("FileToGN() failed: %s", err)
	}
	if diff := cmp.Diff(strings.Join(lines, "\n"), wantGN); diff != "" {
		t.Errorf("FileToGN() diff (-got +want):\n%s", diff)
	}
}

func TestRustTestFfxPluginCrate(t *testing.T) {
	bazel := `# @bazel2gn:skip
ffx_plugin(
	name = "ffx_foo",
	srcs = ["src/lib.rs"],
	args_deps = [":argh"],
	args_srcs = ["src/args.rs"],
	edition = "2024",
	plugin_deps = ["//src/developer/ffx/plugins/bar:ffx_bar"],
	deps = [":fho"],
)

rustc_test(
	name = "ffx_foo_args_test",
	crate = ":ffx_foo_args",
)

rustc_test(
	name = "ffx_foo_test",
	crate = ":ffx_foo",
	deps = [":tempfile"],
)`
	wantGN := `rustc_test("ffx_foo_args_test") {
	sources = [
		"src/args.rs",
	]
	edition = "2024"
	source_root = "src/args.rs"
	deps = [
		":argh",
		":ffx_foo_sub_command",
	]
}
rustc_test("ffx_foo_test") {
	sources = [
		"src/lib.rs",
	]
	edition = "2024"
	deps = [
		":fho",
		":ffx_foo_args",
	] + [
		":tempfile",
	]
}`
	f := toSyntaxFile(t, bazel)
	lines, err := bazel2gn.FileToGN(f)
	if err != nil {
		t.Fatalf("FileToGN() failed: %s", err)
	}
	if diff := cmp.Diff(strings.Join(lines, "\n"), wantGN); diff != "" {
		t.Errorf("FileToGN() diff (-got +want):\n%s", diff)
	}
}

func TestRustTestDisambiguatedOutputName(t *testing.T) {
	bazel := `load("//build/bazel/rules/host_tests:host_rustc_test.bzl", "host_rustc_test")

rustc_library(
	name = "lib",
	srcs = ["src/lib.rs"],
	crate_name = "ffx_product",
	edition = "2024",
)

rustc_test(
	name = "lib_test",
	crate = ":lib",
	crate_name = "ffx_product_lib_test",
)

# @bazel2gn:raw_attribute:output_name = "ffx_product_host_lib_test"
host_rustc_test(
	name = "lib_host_test",
	crate = ":lib",
)`
	wantGN := `rustc_library("lib") {
	sources = [
		"src/lib.rs",
	]
	crate_name = "ffx_product"
	edition = "2024"
}
rustc_test("lib_test") {
	sources = [
		"src/lib.rs",
	]
	edition = "2024"
	output_name = "ffx_product_lib_test"
}
if (is_host) {
	rustc_test("lib_host_test") {
		sources = [
			"src/lib.rs",
		]
		edition = "2024"
		output_name = "ffx_product_host_lib_test"
	}
}`
	f := toSyntaxFile(t, bazel)
	lines, err := bazel2gn.FileToGN(f)
	if err != nil {
		t.Fatalf("FileToGN() failed: %s", err)
	}
	if diff := cmp.Diff(strings.Join(lines, "\n"), wantGN); diff != "" {
		t.Errorf("FileToGN() diff (-got +want):\n%s", diff)
	}
}

func TestRustTestCrateErrors(t *testing.T) {
	for _, tc := range []struct {
		name  string
		bazel string
	}{
		{
			name: "crate in another package",
			bazel: `rustc_test(
	name = "foo_test",
	crate = "//src/foo",
)`,
		},
		{
			name: "undefined crate",
			bazel: `rustc_test(
	name = "foo_test",
	crate = ":foo",
)`,
		},
		{
			name: "generic lib_test without crate_name",
			bazel: `rustc_library(
	name = "lib",
	srcs = ["src/lib.rs"],
	crate_name = "ffx_product",
)

rustc_test(
	name = "lib_test",
	crate = ":lib",
)`,
		},
		{
			name: "generic core_test without crate_name",
			bazel: `rustc_library(
	name = "core",
	srcs = ["src/lib.rs"],
	crate_name = "fdf_core",
)

host_rustc_test(
	name = "core_test",
	crate = ":core",
)`,
		},
		{
			name: "standalone host_rustc_test with generic lib_test name",
			bazel: `host_rustc_test(
	name = "lib_test",
	srcs = ["src/lib.rs"],
	binary_name = "custom_bazel_only_name",
)`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			if _, err := bazel2gn.FileToGN(f); err == nil {
				t.Errorf("FileToGN() unexpectedly succeeded. Bazel source:\n%s", tc.bazel)
			}
		})
	}
}
