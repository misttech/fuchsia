// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn_test

import (
	"testing"

	"github.com/google/go-cmp/cmp"
)

func TestCompiledActionConversion(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "output directory",
			bazel: `compiled_action(
    name = "generate",
    testonly = True,
    outs = [
        "a.far",
        "b.far",
    ],
    args = [
        "--output-dir",
        "$(RULEDIR)",
    ],
    tool = ":generator",
)
`,
			wantGN: `compiled_action("generate") {
	testonly = true
	outputs = [
		"$target_gen_dir/a.far",
		"$target_gen_dir/b.far",
	]
	args = [
		"--output-dir",
		rebase_path(target_gen_dir, root_build_dir),
	]
	tool = ":generator"
}`,
		},
		{
			name: "locations of inputs and outputs",
			bazel: `compiled_action(
    name = "generate",
    srcs = [
        "input.json",
        "//:root_pkg/third_party/foo/data.zip",
        "//path/to:other.txt",
    ],
    outs = ["gen/out.rs"],
    args = [
        "$(location :input.json)",
        "$(execpath //:root_pkg/third_party/foo/data.zip)",
        "--other=$(location //path/to:other.txt)",
        "--out=$(location :gen/out.rs)",
        "--rule-dir=$(RULEDIR)/gen",
    ],
    tool = "//path/to/tool:bin",
    visibility = ["//foo:__pkg__"],
)
`,
			wantGN: `compiled_action("generate") {
	sources = [
		"input.json",
		"//root_pkg/third_party/foo/data.zip",
		"//path/to/other.txt",
	]
	outputs = [
		"$target_gen_dir/gen/out.rs",
	]
	args = [
		rebase_path("input.json", root_build_dir),
		rebase_path("//root_pkg/third_party/foo/data.zip", root_build_dir),
		"--other=" + rebase_path("//path/to/other.txt", root_build_dir),
		"--out=" + rebase_path("$target_gen_dir/gen/out.rs", root_build_dir),
		"--rule-dir=" + rebase_path(target_gen_dir, root_build_dir) + "/gen",
	]
	tool = "//path/to/tool:bin"
	visibility = [
		":*",
		"//foo:*",
	]
}`,
		},
		{
			name: "outputs with path overwrite",
			bazel: `compiled_action(
    name = "generate",
    outs = [
        "out.rs",  # @bazel2gn:path_overwrite:$root_gen_dir/foo/out.rs
    ],
    args = [
        "$(location out.rs)",
    ],
    tool = ":generator",
)
`,
			wantGN: `compiled_action("generate") {
	outputs = [
		"$root_gen_dir/foo/out.rs",
	]
	args = [
		rebase_path("$root_gen_dir/foo/out.rs", root_build_dir),
	]
	tool = ":generator"
}`,
		},
		{
			name: "literal arguments are escaped",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = [
        "$HOME",
        "say \"hi\"",
        "a\\b",
        "",
    ],
    tool = ":generator",
)
`,
			wantGN: `compiled_action("generate") {
	outputs = [
		"$target_gen_dir/out.txt",
	]
	args = [
		"\$HOME",
		"say \"hi\"",
		"a\\b",
		"",
	]
	tool = ":generator"
}`,
		},
		{
			name: "third-party tool",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = ["$(location :out.txt)"],
    tool = "//third_party/rust_crates/vendor:some-tool",
)
`,
			wantGN: `compiled_action("generate") {
	outputs = [
		"$target_gen_dir/out.txt",
	]
	args = [
		rebase_path("$target_gen_dir/out.txt", root_build_dir),
	]
	tool = "//third_party/rust_crates:some-tool"
}`,
		},
		{
			name: "raw overwrite of args",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = ["$(location :out.txt)"],  # @bazel2gn:raw_overwrite:rebase_path(outputs, root_build_dir)
    tool = ":generator",
)
`,
			wantGN: `compiled_action("generate") {
	outputs = [
		"$target_gen_dir/out.txt",
	]
	args = rebase_path(outputs, root_build_dir)
	tool = ":generator"
}`,
		},
		{
			name: "raw overwrite of args element",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = [
        "--out",
        "$(location :out.txt)",  # @bazel2gn:raw_overwrite:rebase_path(outputs[0], root_build_dir)
    ],
    tool = ":generator",
)
`,
			wantGN: `compiled_action("generate") {
	outputs = [
		"$target_gen_dir/out.txt",
	]
	args = [
		"--out",
		rebase_path(outputs[0], root_build_dir),
	]
	tool = ":generator"
}`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			gotGN, err := bazelToGN(f)
			if err != nil {
				t.Fatalf("Unexpected failure converting Bazel targets: %v", err)
			}
			if diff := cmp.Diff(tc.wantGN, gotGN); diff != "" {
				t.Errorf("Diff found after converting Bazel targets (-want +got):\n%s", diff)
			}
		})
	}
}

func TestCompiledActionConversionErrors(t *testing.T) {
	for _, tc := range []struct {
		name  string
		bazel string
	}{
		{
			name: "unsupported make variable",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = ["$(SRCS)"],
    tool = ":generator",
)`,
		},
		{
			name: "unsupported plural location",
			bazel: `compiled_action(
    name = "generate",
    srcs = [":inputs"],
    outs = ["out.txt"],
    args = ["$(locations :inputs)"],
    tool = ":generator",
)`,
		},
		{
			name: "unterminated substitution",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = ["$(location :out.txt"],
    tool = ":generator",
)`,
		},
		{
			name: "external repository label",
			bazel: `compiled_action(
    name = "generate",
    srcs = ["@foo//:bar.txt"],
    outs = ["out.txt"],
    args = ["$(location @foo//:bar.txt)"],
    tool = ":generator",
)`,
		},
		{
			name: "label without target name",
			bazel: `compiled_action(
    name = "generate",
    srcs = ["//foo"],
    outs = ["out.txt"],
    args = ["$(location //foo)"],
    tool = ":generator",
)`,
		},
		{
			name: "args not a list literal",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = ARGS,
    tool = ":generator",
)`,
		},
		{
			name: "non-literal arg",
			bazel: `compiled_action(
    name = "generate",
    outs = ["out.txt"],
    args = [ARG],
    tool = ":generator",
)`,
		},
		{
			name: "outs not a list literal",
			bazel: `compiled_action(
    name = "generate",
    outs = OUTS,
    args = [],
    tool = ":generator",
)`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			_, err := bazelToGN(f)
			if err == nil {
				t.Errorf("Unexpected success converting Bazel targets. Bazel source:\n%s", tc.bazel)
			}
		})
	}
}
