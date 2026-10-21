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
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			if _, err := bazel2gn.FileToGN(f); err == nil {
				t.Errorf("FileToGN() unexpectedly succeeded. Bazel source:\n%s", tc.bazel)
			}
		})
	}
}
