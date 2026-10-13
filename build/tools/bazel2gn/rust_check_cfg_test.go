// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn_test

import (
	"testing"

	"github.com/google/go-cmp/cmp"
)

func TestRustcCheckCfgBuildFlags(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "rustc_check_cfg_build_flags with cfgs and features",
			bazel: `rustc_check_cfg_build_flags(
    name = "check_cfg_flags",
    cfgs = ["test"],
    features = ["benchmark"],
)
`,
			wantGN: `config("check_cfg_flags") {
	rustflags = [
		"--check-cfg=cfg(test)",
		"--check-cfg=cfg(feature, values(\"benchmark\"))",
	]
}`,
		},
		{
			name: "rustc_check_cfg_build_flags with cfgs only",
			bazel: `rustc_check_cfg_build_flags(
    name = "check_cfg_flags",
    cfgs = [
        "test",
        "loom",
    ],
)
`,
			wantGN: `config("check_cfg_flags") {
	rustflags = [
		"--check-cfg=cfg(test)",
		"--check-cfg=cfg(loom)",
	]
}`,
		},
		{
			name: "rustc_check_cfg_build_flags with visibility",
			bazel: `rustc_check_cfg_build_flags(
    name = "check_cfg_flags",
    cfgs = ["test"],
    visibility = ["//visibility:public"],
)
`,
			wantGN: `config("check_cfg_flags") {
	rustflags = [
		"--check-cfg=cfg(test)",
	]
	visibility = [
		"*",
	]
}`,
		},
		{
			name: "rustc_check_cfg_build_flags referenced in rustc_library",
			bazel: `rustc_check_cfg_build_flags(
    name = "check_cfg_flags",
    cfgs = ["test"],
    features = ["benchmark"],
)

rustc_library(
    name = "lib",
    srcs = ["src/lib.rs"],
    build_flags = [":check_cfg_flags"],
)
`,
			wantGN: `config("check_cfg_flags") {
	rustflags = [
		"--check-cfg=cfg(test)",
		"--check-cfg=cfg(feature, values(\"benchmark\"))",
	]
}
rustc_library("lib") {
	sources = [
		"src/lib.rs",
	]
	configs += [
		":check_cfg_flags",
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
