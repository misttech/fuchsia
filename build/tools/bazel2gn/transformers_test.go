// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn_test

import (
	"testing"

	"github.com/google/go-cmp/cmp"
)

func TestVisibilityConversion(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "public",
			bazel: `go_library(
	name = "test",
	visibility = [
		"//visibility:public",
	],
)`,
			wantGN: `go_library("test") {
	visibility = [
		"*",
	]
}`,
		},
		{
			name: "private",
			bazel: `go_library(
	name = "test",
	visibility = [
		"//visibility:private",
	],
)`,
			wantGN: `go_library("test") {
	visibility = [
		":*",
	]
}`,
		},
		{
			name: "pkg and subpackages",
			bazel: `go_library(
	name = "test",
	visibility = [
		"//path/to/foo:__pkg__",
		"//path/to/bar:__subpackages__",
	],
)`,
			wantGN: `go_library("test") {
	visibility = [
		"//path/to/foo:*",
		"//path/to/bar/*",
	]
}`,
		},
		{
			name: "package group is unchanged",
			bazel: `go_library(
	name = "test",
	visibility = [
		"//path/to/foo:__pkg__",
		"//path/to/bar:bar",
	],
)`,
			wantGN: `go_library("test") {
	visibility = [
		"//path/to/foo:*",
		"//path/to/bar:bar",
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

func TestDepsConversion(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "rust third-party",
			bazel: `go_library(
	name = "test",
	deps = [
		"//third_party/rust_crates/vendor:foo",
		"//third_party/rust_crates/ask2patch:bar",
		"//third_party/rust_crates/forks/baz-v0.4.2:baz",
		"//path/to/dep",
	],
)`,
			wantGN: `go_library("test") {
	deps = [
		"//third_party/rust_crates:foo",
		"//third_party/rust_crates:bar",
		"//third_party/rust_crates:baz",
		"//path/to/dep",
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
