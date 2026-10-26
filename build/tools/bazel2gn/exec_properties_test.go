// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn_test

import (
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"
)

func TestExecPropertiesConversion(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "large RBE worker on rustc_binary",
			bazel: `rustc_binary(
    name = "bin",
    srcs = ["src/main.rs"],
    build_flags = ["//build/config/rust:bootfs"],
    exec_properties = RBE_USE_LARGE_WORKER,
)
`,
			wantGN: `rustc_binary("bin") {
	sources = [
		"src/main.rs",
	]
	configs += [
		"//build/config/rust:bootfs",
	]
	_concurrent_jobs = concurrent_jobs.rust_highmem
	configs += _concurrent_jobs.configs
	forward_variables_from(_concurrent_jobs.vars, "*")
}`,
		},
		{
			name: "large RBE worker on rustc_test",
			bazel: `rustc_test(
    name = "foo_test",
    srcs = ["src/lib.rs"],
    exec_properties = RBE_USE_LARGE_WORKER,
)
`,
			wantGN: `rustc_test("foo_test") {
	sources = [
		"src/lib.rs",
	]
	_concurrent_jobs = concurrent_jobs.rust_highmem
	configs += _concurrent_jobs.configs
	forward_variables_from(_concurrent_jobs.vars, "*")
}`,
		},
		{
			name: "skipped exec_properties",
			bazel: `rustc_library(
    name = "lib",
    srcs = ["src/lib.rs"],
    exec_properties = {"Pool": "foo"},  # @bazel2gn:skip
)
`,
			wantGN: `rustc_library("lib") {
	sources = [
		"src/lib.rs",
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

func TestExecPropertiesConversionErrors(t *testing.T) {
	for _, tc := range []struct {
		name    string
		bazel   string
		wantErr string
	}{
		{
			name: "dict literal",
			bazel: `rustc_binary(
    name = "bin",
    srcs = ["src/main.rs"],
    exec_properties = {"Pool": "large"},
)`,
			wantErr: `bazel2gn only supports ` + "`" + `exec_properties = RBE_USE_LARGE_WORKER` + "`" + `, got *syntax.DictExpr; use "# @bazel2gn:skip" and "# @bazel2gn:raw_attribute:" instead`,
		},
		{
			name: "other identifier",
			bazel: `rustc_binary(
    name = "bin",
    srcs = ["src/main.rs"],
    exec_properties = SOME_PROPERTIES,
)`,
			wantErr: `bazel2gn only supports ` + "`" + `exec_properties = RBE_USE_LARGE_WORKER` + "`" + `, got SOME_PROPERTIES; use "# @bazel2gn:skip" and "# @bazel2gn:raw_attribute:" instead`,
		},
		{
			name: "select",
			bazel: `rustc_binary(
    name = "bin",
    srcs = ["src/main.rs"],
    exec_properties = select({
        "//conditions:default": RBE_USE_LARGE_WORKER,
    }),
)`,
			wantErr: `bazel2gn only supports ` + "`" + `exec_properties = RBE_USE_LARGE_WORKER` + "`" + `, got *syntax.CallExpr; use "# @bazel2gn:skip" and "# @bazel2gn:raw_attribute:" instead`,
		},
		{
			name: "non-Rust rule",
			bazel: `cc_binary(
    name = "bin",
    srcs = ["main.cc"],
    exec_properties = RBE_USE_LARGE_WORKER,
)`,
			wantErr: `bazel2gn only supports exec_properties on Rust binary, library, and test rules, got cc_binary; use "# @bazel2gn:skip" and "# @bazel2gn:raw_attribute:" instead`,
		},
		{
			name: "raw overwrite",
			bazel: `rustc_binary(
    name = "bin",
    srcs = ["src/main.rs"],
    exec_properties = RBE_USE_LARGE_WORKER,  # @bazel2gn:raw_overwrite:{}
)`,
			wantErr: `"# @bazel2gn:raw_overwrite:" is not supported on exec_properties; use "# @bazel2gn:skip" and "# @bazel2gn:raw_attribute:" instead`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			if _, err := bazelToGN(f); err == nil {
				t.Fatalf("Unexpected success converting Bazel targets. Bazel source:\n%s", tc.bazel)
			} else if !strings.Contains(err.Error(), tc.wantErr) {
				t.Errorf("bazelToGN() error = %q, want substring %q", err.Error(), tc.wantErr)
			}
		})
	}
}
