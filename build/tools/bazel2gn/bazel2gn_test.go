// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn_test

import (
	"fmt"
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

	f, err := bazel2gn.ParseBytes("BUILD.bazel.test", []byte(s))
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
	size = "small",
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
		{
			name: "Aliases",
			bazel: `alias(
	name = "alias_name",
	actual = ":actual_target",
)`,
			wantGN: `group("alias_name") {
	public_deps = [
		":actual_target",
	]
}`,
		},
		{
			name: "AliasesSelect",
			bazel: `alias(
	name = "alias_name",
	actual = select({
		"@platforms//os:fuchsia": ":actual_target_fuchsia",
		"//conditions:default": ":actual_target_default",
	}),
)`,
			wantGN: `group("alias_name") {
	if (is_fuchsia) {
		public_deps = [
			":actual_target_fuchsia",
		]
	} else {
		public_deps = [
			":actual_target_default",
		]
	}
}`,
		},
		{
			name: "fuchsia_structured_config_rust_lib",
			bazel: `load("//build/bazel/rules/components:structured_config.bzl", "fuchsia_structured_config_rust_lib")

fuchsia_structured_config_rust_lib(
	name = "my_component_config",
	cm_label = ":manifest",
)

fuchsia_structured_config_rust_lib(
	name = "my_test_config",
	testonly = True,
	cm_label = ":test_manifest",
	crate_name = "my_crate",
	visibility = ["//visibility:public"],
)`,
			wantGN: `fuchsia_structured_config_rust_lib("my_component_config") {
	cm_label = ":manifest"
}
fuchsia_structured_config_rust_lib("my_test_config") {
	testonly = true
	cm_label = ":test_manifest"
	name = "my_crate"
	visibility = [
		"*",
	]
}`,
		},
		{
			name: "Bazel-only rules with no GN equivalent",
			bazel: `exports_files(["meta/foo.cml"])

fx_packaged_binary(
	name = "foo_packaged_bin",
	binary = ":foo_bin",
	binary_name = "foo",
)

fx_test_component(
	name = "foo-test-component",
	compiled_manifest = ":foo-manifest",
	component_name = "foo-test",
	deps = [":foo_packaged_bin"],
)

# Not referenced by an fx_package with test_components.
fx_test_component(
	name = "unpackaged-test-component",
	compiled_manifest = ":foo-manifest",
)

fx_package(
	name = "foo-package",
	package_name = "foo",
	components = [":foo-component"],
)

fx_package(
	name = "foo-test-package",
	package_name = "foo-test",
	test_components = [
		# @bazel2gn:skip
		":foo-test-component",
	],
)

# @bazel2gn:skip
fx_package(
	name = "skipped-test-package",
	test_components = [":foo-test-component"],
)

fx_test(
	name = "foo-test",
	package = ":foo-test-package",
)`,
			wantGN: "",
		},
		{
			name: "fx_component_manifest and fx_component",
			bazel: `fx_component_manifest(
	name = "manifest",
	component_name = "my-component",
	includes = [
		"//sdk/lib/syslog:client.shard",
		"//sdk/lib/inspect:client.shard.cml",
	],
	manifest = ":meta/my_component.cml",
)

fx_component(
	name = "component",
	check_includes = False,
	compiled_manifest = ":manifest",
	component_name = "my-component",
	visibility = ["//bundles/assembly:__subpackages__"],
	deps = ["//src/starnix/containers:default_init"],
)`,
			wantGN: `fuchsia_component_manifest("manifest") {
	component_name = "my-component"
	manifest = "meta/my_component.cml"
}
fuchsia_component("component") {
	check_includes = false
	cm_label = ":manifest"
	component_name = "my-component"
	visibility = [
		":*",
		"//bundles/assembly/*",
	]
	deps = [
		"//src/starnix/containers:default_init",
	]
}`,
		},
		{
			name: "Ignored unittest targets",
			bazel: `fx_unittest_component(
	name = "foo-test-component",
	component_name = "foo-test",
	binary = ":foo_bin",
)`,
			wantGN: "",
		},
		{
			name: "fx_unittest_package",
			bazel: `fx_unittest_package(
	name = "foo-test",
	package_name = "foo-tests",
	unit_tests = [":foo_bin"],
	max_log_severity = "ERROR",
	tags = ["manual"],
	visibility = ["//src/foo:__pkg__"],
)`,
			wantGN: `fuchsia_unittest_package("foo-test") {
	package_name = "foo-tests-gn"
	component_name = "foo-tests"
	deps = [
		":foo_bin",
	]
	test_specs = {
		log_settings = {
			max_severity = "ERROR"
		}
	}
	visibility = [
		":*",
		"//src/foo:*",
	]
}`,
		},
		{
			name: "fx_unittest_package without package_name",
			bazel: `fx_unittest_package(
	name = "foo-test",
	unit_tests = ["//src/foo:foo_bin"],
)`,
			wantGN: `fuchsia_unittest_package("foo-test") {
	package_name = "foo-test-gn"
	component_name = "foo-test"
	deps = [
		"//src/foo:foo_bin",
	]
}`,
		},
		{
			name: "fx_unittest_package with multiple unit tests",
			bazel: `fx_unittest_package(
	name = "foo-test",
	package_name = "foo-tests",
	unit_tests = [
		":foo_test",
		"//src/foo/bar:bar_test",
	],
)`,
			wantGN: `fuchsia_unittest_component("foo-test.foo_test.component") {
	component_name = "foo_test"
	visibility = [ ":*" ]
	deps = [
		":foo_test",
	]
}
fuchsia_unittest_component("foo-test.bar_test.component") {
	component_name = "bar_test"
	visibility = [ ":*" ]
	deps = [
		"//src/foo/bar:bar_test",
	]
}
fuchsia_test_package("foo-test") {
	package_name = "foo-tests-gn"
	test_components = [
		":foo-test.foo_test.component",
		":foo-test.bar_test.component",
	]
}`,
		},
		{
			name: "fx_unittest_package with skipped unit test",
			bazel: `fx_unittest_package(
	name = "foo-test",
	package_name = "foo-tests",
	unit_tests = [
		":foo_test",
		# @bazel2gn:skip
		":foo_nostd_test",
	],
)`,
			wantGN: `fuchsia_unittest_package("foo-test") {
	package_name = "foo-tests-gn"
	component_name = "foo-tests"
	deps = [
		":foo_test",
	]
}`,
		},
		{
			name: "host_go_test",
			bazel: `host_go_test(
	name = "foo_test",
	srcs = ["foo_test.go"],
	binary_name = "foo_test_bin",
	embed = [":foo"],
	size = "small",
	test_args = ["--foo"],
	timeout = "10m",
)`,
			wantGN: `if (is_host) {
	go_test("foo_test") {
		sources = [
			"foo_test.go",
		]
		embed = [
			":foo",
		]
		args = [
			"--foo",
		]
		timeout = "10m"
	}
}`,
		},
		{
			name: "host_rustc_test",
			bazel: `host_rustc_test(
	name = "foo_test",
	srcs = ["src/lib.rs"],
	crate_root = "src/lib.rs",
	edition = "2024",
	test_args = ["--foo"],
)`,
			wantGN: `if (is_host) {
	rustc_test("foo_test") {
		sources = [
			"src/lib.rs",
		]
		source_root = "src/lib.rs"
		edition = "2024"
		args = [
			"--foo",
		]
	}
}`,
		},
		{
			name: "host_go_test with target_compatible_with",
			bazel: `host_go_test(
	name = "foo_test",
	srcs = ["foo_test.go"],
	target_compatible_with = ["@platforms//os:fuchsia"],
)`,
			wantGN: `if (is_fuchsia && is_host) {
	go_test("foo_test") {
		sources = [
			"foo_test.go",
		]
	}
}`,
		},
		{
			name: "Ignored host test rules",
			bazel: `wrap_host_rust_test(
	name = "foo_host_test",
	test = ":foo_test",
)

wrap_host_rust_test(
	name = "foo_wrapped_rust_test",
	binary_name = "foo_rust_test",
)

host_cc_test(
	name = "foo_cc_test",
	srcs = ["foo_test.cc"],
)

host_py_test(
	name = "foo_py_test",
	main = "foo_test.py",
)

host_shac_test(
	name = "foo_shac_test",
	srcs = ["foo_test.star"],
)

host_test(
	name = "foo_test",
	binary = ":foo_bin",
)

host_test_data_files(
	name = "foo_test_data",
	srcs = ["testdata/foo.json"],
)`,
			wantGN: "",
		},
		{
			name: "Rust proto library",
			bazel: `load("//tools/protobuf:rust_proto_library.bzl", "rust_proto_library")

rust_proto_library(
	name = "simple_proto",
	srcs = ["src/simple.proto"],
	packages = ["fuchsia.simple"],
	visibility = [":__pkg__"],
)

rust_proto_library(
	name = "foo_proto",
	srcs = [
		"//third_party/foo/protos:a.proto",
		"//third_party/foo/protos:b.proto",
	],
	packages = [
		"foo.protos",
		"google.protobuf",
	],
	include_dirs = ["//third_party/foo"],
	proto_deps = ["//third_party/foo/protos:imported_protos"],
	deps = ["//third_party/rust_crates/vendor:bytes"],
)`,
			wantGN: `rust_proto_library("simple_proto") {
	sources = [
		"src/simple.proto",
	]
	packages = [
		"fuchsia.simple",
	]
	visibility = [
		":*",
	]
}
rust_proto_library("foo_proto") {
	sources = [
		"//third_party/foo/protos/a.proto",
		"//third_party/foo/protos/b.proto",
	]
	packages = [
		"foo.protos",
		"google.protobuf",
	]
	include_dirs = [
		"//third_party/foo",
	]
	deps = [
		"//third_party/rust_crates:bytes",
	]
}`,
		},
		{
			name: "Measure tapes",
			bazel: `load("//tools/fidl/measure-tape:measure_tape.bzl", "measure_tape")

measure_tape(
	name = "hlcpp_measure_tape_for_foo",
	fidls = [
		":fuchsia.foo",
		"//sdk/fidl/fuchsia.math",
	],
	target_binding = "hlcpp",
	target_types = [
		"fuchsia.foo/Foo",
		"fuchsia.foo/Bar",
	],
)

measure_tape(
	name = "rust_measure_tape_for_foo",
	fidls = ["//sdk/fidl/fuchsia.foo"],
	target_binding = "rust",
	target_types = ["fuchsia.foo/Foo"],
	testonly = True,
	visibility = ["//visibility:public"],
)`,
			wantGN: `measure_tape("hlcpp_measure_tape_for_foo") {
	fidls = [
		":fuchsia.foo",
		"//sdk/fidl/fuchsia.math",
	]
	target_binding = "hlcpp"
	target_types = [
		"fuchsia.foo/Foo",
		"fuchsia.foo/Bar",
	]
}
measure_tape("rust_measure_tape_for_foo") {
	fidls = [
		"//sdk/fidl/fuchsia.foo",
	]
	target_binding = "rust"
	target_types = [
		"fuchsia.foo/Foo",
	]
	testonly = true
	visibility = [
		"*",
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
				t.Errorf("Unexpected success converting Bazel targets. Bazel source:\n%s", tc.bazel)
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
			name: "visibility transform of file variable",
			bazel: `# @bazel2gn:transformer=visibility
_foo_visibility = [
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
		}, {
			name: "rustc_dylib",
			bazel: `
rustc_dylib(
	name = "foo_dylib",
	srcs = ["src/lib.rs"],
	build_flags = ["//build/config/rust:bootfs"],
	crate_features = ["bar"],
	crate_name = "foo",
	output_name = "foo_rust",
	target_compatible_with = ["@platforms//os:fuchsia"],
	with_unit_tests = True,
	deps = ["//third_party/rust_crates/vendor:anyhow"],
)`,
			wantGN: `if (is_fuchsia) {
	rustc_dylib("foo_dylib") {
		sources = [
			"src/lib.rs",
		]
		configs += [
			"//build/config/rust:bootfs",
		]
		features = [
			"bar",
		]
		crate_name = "foo"
		output_name = "foo_rust"
		with_unit_tests = true
		deps = [
			"//third_party/rust_crates:anyhow",
		]
	}
}`,
		}, {
			name: "rustc_disable_clippy",
			bazel: `
rustc_library(
	name = "foo_lib",
	srcs = ["src/lib.rs"],
	disable_clippy = True,
)

rustc_binary(
	name = "foo_bin",
	srcs = ["src/main.rs"],
	disable_clippy = True,
)

rustc_dylib(
	name = "foo_dylib",
	srcs = ["src/lib.rs"],
	disable_clippy = True,
)

rustc_proc_macro(
	name = "foo_macro",
	srcs = ["src/lib.rs"],
	disable_clippy = True,
)

rustc_test(
	name = "foo_test",
	srcs = ["src/main.rs"],
	crate_name = "foo_test_bin",
	disable_clippy = True,
)`,
			wantGN: `rustc_library("foo_lib") {
	sources = [
		"src/lib.rs",
	]
	disable_clippy = true
}
rustc_binary("foo_bin") {
	sources = [
		"src/main.rs",
	]
	disable_clippy = true
}
rustc_dylib("foo_dylib") {
	sources = [
		"src/lib.rs",
	]
	disable_clippy = true
}
rustc_macro("foo_macro") {
	sources = [
		"src/lib.rs",
	]
	disable_clippy = true
}
rustc_test("foo_test") {
	sources = [
		"src/main.rs",
	]
	output_name = "foo_test_bin"
	disable_clippy = true
}`,
		}, {
			name: "ffx_plugin",
			bazel: `
ffx_plugin(
	name = "ffx_foo",
	args_crate_root = "src/foo_args.rs",
	args_deps = ["//third_party/rust_crates/vendor:argh"],
	args_srcs = ["src/foo_args.rs"],
	compile_data = ["data/foo.txt"],
	config_data = ["data/config.json"],
	crate_features = ["bar"],
	data = ["//src/foo:data"],
	disable_clippy = True,
	edition = "2024",
	lint_config = "//build/config/rust/lints:deny_unused_results",
	link_deps = ["//sdk/lib/fdio"],
	plugin_deps = ["//src/developer/ffx/plugins/foo/bar:ffx_foo_bar"],
	proc_macro_deps = ["//third_party/rust_crates/vendor:async-trait"],
	sdk_category = "partner",
	srcs = ["src/lib.rs"],
	version = "0.1.0",
	deps = ["//third_party/rust_crates/vendor:anyhow"],
)

ffx_tool(
	name = "ffx_foo_tool",
	srcs = ["src/main.rs"],
	binary_name = "ffx-foo",
	disable_instrumentation = True,
	edition = "2024",
	deps = [":ffx_foo"],
)

idk_ffx_tool(
	name = "ffx_foo_partner_tool",
	srcs = ["src/main.rs"],
	category = "partner",
	edition = "2024",
	idk_name = "ffx-foo-partner",
	deps = [":ffx_foo"],
)`,
			wantGN: `ffx_plugin("ffx_foo") {
	args_source_root = "src/foo_args.rs"
	args_deps = [
		"//third_party/rust_crates:argh",
	]
	args_sources = [
		"src/foo_args.rs",
	]
	inputs = [
		"data/foo.txt",
	]
	config_data = [
		"data/config.json",
	]
	features = [
		"bar",
	]
	non_rust_deps = [
		"//src/foo:data",
	]
	disable_clippy = true
	edition = "2024"
	lint_config = "//build/config/rust/lints:deny_unused_results"
	link_deps = [
		"//sdk/lib/fdio",
	]
	plugin_deps = [
		"//src/developer/ffx/plugins/foo/bar:ffx_foo_bar",
	]
	proc_macro_deps = [
		"//third_party/rust_crates:async-trait",
	]
	sdk_category = "partner"
	sources = [
		"src/lib.rs",
	]
	version = "0.1.0"
	deps = [
		"//third_party/rust_crates:anyhow",
	]
}`,
		}, {
			name: "rustc_library link_deps",
			bazel: `
rustc_library(
	name = "foo",
	link_deps = [
		"//zircon/system/ulib/sync",
		"//sdk/lib/zxio",  # @bazel2gn:path_overwrite://sdk/lib/zxio:zxio_static
	] + select({
		"@platforms//os:fuchsia": ["//sdk/lib/fdio"],
		"//conditions:default": [],
	}),
	deps = ["//third_party/rust_crates/vendor:anyhow"],
)`,
			wantGN: `rustc_library("foo") {
	link_deps = []
	link_deps += [
		"//zircon/system/ulib/sync",
		"//sdk/lib/zxio:zxio_static",
	]
	if (is_fuchsia) {
		link_deps += [
			"//sdk/lib/fdio",
		]
	}
	deps = [
		"//third_party/rust_crates:anyhow",
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
		"-Wl,--version-script=sdk/lib/fdio/fdio.ld",  # @bazel2gn:raw_overwrite:"-Wl,--version-script=" + rebase_path("fdio.ld", root_build_dir)
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
			name: "configs clearing annotation",
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
			// The generated GN could be invalid because `configs` usually is not empty.
			// TODO(https://fxbug.dev/543568916): This should probably fail since `configs` is
			// never/rarely empty in Fuchsia GN builds.
			name: "configs clearing annotation with non-empty copts",
			bazel: `cc_library(
	name = "clear_configs_with_non_empty_copts",
	copts = [
		"-Wno-implicit-fallthrough",
	], # @bazel2gn:clear
)
`,
			wantGN: `static_library("clear_configs_with_non_empty_copts") {
	configs = [
		"//build/config:Wno-implicit-fallthrough",
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
		{
			name: "fx_cc_binary with build_flags and instrumentation disabled",
			bazel: `fx_cc_binary(
	name = "foo",
	srcs = ["foo.cc"],
	build_flags = [
		"//build/config/fuchsia:no_cpp_standard_library",
		":config",
	],
	disable_build_flags = ["//build/config/linux:implicit-host-libs"],
	disable_instrumentation = True,
	disable_syslog_backend = True,
)
`,
			wantGN: `executable("foo") {
	sources = [
		"foo.cc",
	]
	configs += [
		"//build/config/fuchsia:no_cpp_standard_library",
		":config",
	]
	configs -= [
		"//build/config/linux:implicit-host-libs",
	]
	exclude_toolchain_tags = [ "instrumented" ]
	disable_syslog_backend = true
}`,
		},
		{
			name: "fx_cc_binary disable_instrumentation = False is dropped",
			bazel: `fx_cc_binary(
	name = "foo",
	disable_instrumentation = False,
)
`,
			wantGN: `executable("foo") {
}`,
		},
		{
			name: "fx_cc_library build_flags with path_overwrite",
			bazel: `fx_cc_library(
	name = "foo",
	build_flags = [
		"//build/config:foo",  # @bazel2gn:path_overwrite://build/config:gn_foo
	],
)
`,
			wantGN: `static_library("foo") {
	configs += [
		"//build/config:gn_foo",
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

func TestRustBindgenGoldenConversion(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "rustc_bindgen_golden",
			bazel: `load("//build/bazel/rules/rust:rustc_bindgen.bzl", "rustc_bindgen_golden")

rustc_bindgen_golden(
    name = "foo_bindgen_golden",
    checked_in_source = "src/bindings.rs",
    expect_lints = ["non_camel_case_types"],
    function_allowlist = ["foo_.*"],
    headers = [
        "//sdk/lib/foo/include/lib/foo/foo.h",
        "//sdk/lib/foo/include/lib/foo/bar.h",
    ],
    layout_tests = True,
    notice_year = 2026,
    deps = ["//sdk/lib/foo"],
)
`,
			wantGN: `rustc_bindgen_golden("foo_bindgen_golden") {
	checked_in_source = "src/bindings.rs"
	expect_lints = [
		"non_camel_case_types",
	]
	function_allowlist = [
		"foo_.*",
	]
	headers = [
		"//sdk/lib/foo/include/lib/foo/foo.h",
		"//sdk/lib/foo/include/lib/foo/bar.h",
	]
	layout_tests = true
	notice_year = 2026
	deps = [
		"//sdk/lib/foo",
	]
}`,
		},
		{
			name: "rustc_library with validations",
			bazel: `load("//build/bazel/rules/rust:rustc_library.bzl", "rustc_library")

rustc_library(
    name = "foo-sys",
    srcs = ["src/lib.rs"],
    edition = "2024",
    validations = [":foo_bindgen_golden"],
)
`,
			wantGN: `rustc_library("foo-sys") {
	sources = [
		"src/lib.rs",
	]
	edition = "2024"
	validations = [
		":foo_bindgen_golden",
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

func TestUnsupportedTestAttributes(t *testing.T) {
	for _, tc := range []struct {
		name  string
		bazel string
	}{
		{
			name: "host_go_test with test_data",
			bazel: `host_go_test(
	name = "foo_test",
	srcs = ["foo_test.go"],
	test_data = [":testdata"],
)`,
		},
		{
			name: "host_rustc_test with test_data",
			bazel: `host_rustc_test(
	name = "foo_test",
	srcs = ["src/lib.rs"],
	test_data = [":testdata"],
)`,
		},
		{
			name: "fx_unittest_package with non-literal unit tests",
			bazel: `fx_unittest_package(
	name = "foo-test",
	unit_tests = [":foo_bin", BAR_TEST],
)`,
		},
		{
			name: "fx_unittest_package with environments",
			bazel: `fx_unittest_package(
	name = "foo-test",
	unit_tests = [":foo_bin"],
	environments = ["//build/testing/environments:emu_env"],
)`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			if _, err := bazelToGN(f); err == nil {
				t.Errorf("Unexpected success converting Bazel targets. Bazel source:\n%s", tc.bazel)
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
				t.Errorf("Unexpected success converting Bazel targets. Bazel source:\n%s", tc.bazel)
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
    cgo = "skipped",  # @bazel2gn:skip
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
		# The API level number below is meant to stand out and look somewhat like "SKIPPED".
		"-ffuchsia-api-level=5718830",  # @bazel2gn:skip
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

func TestPathOverwriteAnnotations(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			// Some instances of `path_overwrite` are ignored because it is only handled for
			// specific attributes. Specifically, `copts` and `visibility` list items are not overwritten.
			// TODO(https://fxbug.dev/543568916): This should fail or overwrite all of the list elements.
			// If it is made to fail, break out the test of the supported attributes into a passing
			// test and break out all the unsupported attributes into separate failing tests.
			name: "unexpected success converting: path_overwrite on cc attribute list items",
			bazel: `cc_library(
	name = "foo",
	srcs = [
		"path/to/bar.cc",
		"path/to/bar.h",  # @bazel2gn:path_overwrite:path/to/overwritten_internal.h
		"path/to/baz.cc",
		"path/to/skipped.cc",  # @bazel2gn:skip
		"yet/another/path/to/foo.cc",
	],
	hdrs = [
		"path/to/baz.h",  # @bazel2gn:path_overwrite:path/overwritten.h
		"path/to/foo.h",
	],  # @bazel2gn:skip
	deps = [
		"//path/to:foo",
		"//yet/another/path/to:bar",  # @bazel2gn:path_overwrite://yet/another/path/to:overwritten
	],
	# The overwrite value demonstrates that quotes are added even when the path is already quoted
	# but results in invalid GN.
	implementation_deps = [
		"//path/to:bar",  # @bazel2gn:path_overwrite:"//path/to_gn_lib:bar"
	],
	copts = [
		"-Wno-implicit-fallthrough",  # @bazel2gn:path_overwrite://build/config:ignored
	],
	visibility = [
		":__pkg__",  # @bazel2gn:path_overwrite://path/to/ignored/*
		"//path/to/dir:__subpackages__",
	],
)
`,
			wantGN: `static_library("foo") {
	sources = [
		"path/to/bar.cc",
		"path/to/overwritten_internal.h",
		"path/to/baz.cc",
		"yet/another/path/to/foo.cc",
	]
	public_deps = [
		"//path/to:foo",
		"//yet/another/path/to:overwritten",
	]
	deps = [
		""//path/to_gn_lib:bar"",
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
			// `path_overwrite` is ignored because `attrAssignmentToGN()` does not check for
			// `path_overwrite` annotations on the right hand side.
			// TODO(https://fxbug.dev/543568916): This should fail.
			name: "unexpected success converting: path_overwrite on cc attributes",
			bazel: `cc_library(
	name = "foo",  # @bazel2gn:path_overwrite:ignored_name
	# The overwrite value would demonstrate whether escape characters are converted.
	srcs = [
		"path/to/bar.cc",
		"path/to/bar.h",
		"path/to/baz.cc",
		"path/to/skipped.cc",  # @bazel2gn:skip
		"yet/another/path/to/foo.cc",
	],  # @bazel2gn:path_overwrite:[\n  "path/to/overwritten_bar.cc",\n  "path/to/overwritten_baz.cc"\n]
	hdrs = [
		"path/to/baz.h",  # @bazel2gn:path_overwrite:should_be_ignored.h
		"path/to/foo.h",
	],  # @bazel2gn:path_overwrite:[ "path/to/overwritten.h" ]
	deps = [
		"//path/to:foo",
		"//yet/another/path/to:bar",
	],  # @bazel2gn:path_overwrite:[ "//yet/another/path/to:overwritten" ]
	implementation_deps = [
		"//path/to:bar",
	],  # @bazel2gn:path_overwrite:[ "//path/to_gn_lib:bar" ]
	copts = [
		"-Wno-implicit-fallthrough",
	],  # @bazel2gn:path_overwrite:[ "//build/config:ignored" ]
	visibility = [
		":__pkg__",
		"//path/to/dir:__subpackages__",
	],  # @bazel2gn:path_overwrite:[ "//path/to/ignored/*" ]
)
`,
			wantGN: `static_library("foo") {
	sources = [
		"path/to/bar.cc",
		"path/to/bar.h",
		"path/to/baz.cc",
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
			// `path_overwrite` is ignored because `assignStmtToGN()`, which handles file-level variable
			// assignments, converts `stmt.LHS` and `stmt.RHS` separately and does not check for
			// `path_overwrite` annotations on the statement level.
			// TODO(https://fxbug.dev/543568916): This should fail or generate
			// `zbi_source_file = "overwritten.fidl"`.
			name: "unexpected success converting: path_overwrite on file-level variable assignment",
			bazel: `zbi_source_file = "board.fidl"  # @bazel2gn:path_overwrite:overwritten.fidl

fidl_library(
	name = "zbi",
	srcs = [zbi_source_file],
)
`,
			wantGN: `zbi_source_file = "board.fidl"

# To avoid "Assignment had no effect" from GN.
# It's possible this variable is only used in if conditions (e.g. is_host).
not_needed([ "zbi_source_file" ])

fidl("zbi") {
	sources = [
		zbi_source_file,
	]
}`,
		},
		{
			// `path_overwrite` is ignored because `rustenvToGN()`, which handles the `rustc_env`
			// attribute, only converts keys and values and does not check for `path_overwrite`
			// annotations on dictionary entries.
			// TODO(https://fxbug.dev/543568916): This should fail.
			name: "unexpected success converting: path_overwrite on rustc_env dictionary entry",
			bazel: `rustc_library(
	name = "lib",
	rustc_env = {
		"FOO": "bar",  # @bazel2gn:path_overwrite:FOO=overwritten
	}
)
`,
			wantGN: `rustc_library("lib") {
	rustenv = [
		"FOO=bar",
	]
}`,
		},
		{
			// `path_overwrite` is ignored because target name extraction only converts `binaryExpr.Y`
			// (the RHS literal) and does not check for `path_overwrite` annotations on the
			// `name = "..."` assignment statement.
			// TODO(https://fxbug.dev/543568916): This should fail.
			name: "unexpected success converting: path_overwrite on name attribute",
			bazel: `go_library(
	name = "test",  # @bazel2gn:path_overwrite:overwritten_name
)
`,
			wantGN: `go_library("test") {
}`,
		},
		{
			name: "path_overwrite inside select() statement - on list item",
			bazel: `go_library(
	name = "test",
	deps = select({
		"@platforms//os:fuchsia": [
			"//src:foo"  # @bazel2gn:path_overwrite://src/overwritten/fuchsia
		],
		"//conditions:default": [
			"//src:host",  # @bazel2gn:path_overwrite://src/overwritten/host
		],
	}),
)
`,
			wantGN: `go_library("test") {
	if (is_fuchsia) {
		deps = [
			"//src/overwritten/fuchsia",
		]
	} else {
		deps = [
			"//src/overwritten/host",
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

// Tests use of `raw_overwrite` annotations.
// See `TestVisibilityConversion` for tests of `raw_overwrite` use in and on `visibility`.
func TestRawOverwriteAnnotations(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			// Except for `visibility`, `raw_overwrite` is ignored because `bazelFilePathsToGN()` and
			// `bazelDepToGN()` only handle `path_overwrite`, and some attribute lists are not checked
			// for annotations at all.
			// TODO(https://fxbug.dev/543568916): This should fail or overwrite all of the list elements.
			// If it is made to fail, break out the test of the supported attributes into a passing
			// test and break out all the unsupported attributes into separate failing tests.
			name: "unexpected success converting: raw_overwrite on cc attribute list items",
			bazel: `cc_library(
	name = "foo",
	srcs = [
		"path/to/bar.cc",
		"path/to/bar.h",  # @bazel2gn:raw_overwrite:"path/to/ignored.h"
		"path/to/baz.cc",
		"path/to/skipped.cc",  # @bazel2gn:skip
		"yet/another/path/to/foo.cc",
	],
	hdrs = [
		"path/to/baz.h",  # @bazel2gn:raw_overwrite:"path/ignored.h"
		"path/to/foo.h",
	],  # @bazel2gn:skip
	deps = [
		"//path/to:foo",
		"//yet/another/path/to:bar",  # @bazel2gn:raw_overwrite:"//yet/another/path/to:ignored"
	],
	implementation_deps = [
		"//path/to:bar",  # @bazel2gn:raw_overwrite:"//path/to/ignored:bar"
	],
	copts = [
		"-Wno-implicit-fallthrough",  # @bazel2gn:raw_overwrite://build/config:ignored
	],
	visibility = [
		":__pkg__",  # @bazel2gn:raw_overwrite:"./*"
		"//path/to/dir:__subpackages__",
	],
)
`,
			wantGN: `static_library("foo") {
	sources = [
		"path/to/bar.cc",
		"path/to/bar.h",
		"path/to/baz.cc",
		"yet/another/path/to/foo.cc",
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
		"./*",
		"//path/to/dir/*",
	]
}`},
		{
			// `raw_overwrite` is ignored because `attrAssignmentToGN()` does not check for
			// `raw_overwrite` annotations on the right hand side.
			// TODO(https://fxbug.dev/543568916): This should fail or overwrite the list elements.
			name: "unexpected success converting: raw_overwrite on cc attributes",
			bazel: `cc_library(
	name = "foo",  # @bazel2gn:path_overwrite:ignored_name
	# The overwrite value demonstrates that escape characters are not converted but results
	# in invalid GN because these characters are present in the converted target.
	srcs = [
		"path/to/bar.cc",
		"path/to/bar.h",
		"path/to/baz.cc",
		"path/to/skipped.cc",  # @bazel2gn:skip
		"yet/another/path/to/foo.cc",
	],  # @bazel2gn:raw_overwrite:[\n  "path/to/overwritten_bar.cc",\n  "path/to/overwritten_baz.cc"\n]
	hdrs = [
		"path/to/baz.h",  # @bazel2gn:raw_overwrite:"should_be_ignored.h"
		"path/to/foo.h",
	],  # @bazel2gn:raw_overwrite:[ "path/to/overwritten.h" ]
	deps = [
		"//path/to:foo",
		"//yet/another/path/to:bar",
	],  # @bazel2gn:raw_overwrite:[ "//yet/another/path/to:overwritten" ]
	implementation_deps = [
		"//path/to:bar",
	],  # @bazel2gn:raw_overwrite:[ "//path/to_gn_lib:bar" ]
	copts = [
		"-Wno-implicit-fallthrough",
	],  # @bazel2gn:raw_overwrite:[]
	visibility = [
		":__pkg__",
		"//path/to/dir:__subpackages__",
	],  # @bazel2gn:raw_overwrite:[ "*" ]
)
`,
			wantGN: `static_library("foo") {
	sources = [\n  "path/to/overwritten_bar.cc",\n  "path/to/overwritten_baz.cc"\n]
	public = [ "path/to/overwritten.h" ]
	public_deps = [ "//yet/another/path/to:overwritten" ]
	deps = [ "//path/to_gn_lib:bar" ]
	configs += []
	visibility = [ "*" ]
}`,
		},
		{
			// `raw_overwrite` is ignored because `assignStmtToGN()`, which handles file-level variable
			// assignments, converts `stmt.LHS` and `stmt.RHS` separately and does not check for
			// `raw_overwrite` annotations on the statement level.
			// TODO(https://fxbug.dev/543568916): This should fail or generate
			// `zbi_sources = [ "overwritten.fidl" ]`.
			name: "unexpected success converting: raw_overwrite on file-level variable assignment",
			bazel: `zbi_sources = [
	"board.fidl",
]  # @bazel2gn:raw_overwrite:[ "overwritten.fidl" ]

fidl_library(
	name = "zbi",
	srcs = zbi_sources,
)
`,
			wantGN: `zbi_sources = [
	"board.fidl",
]

# To avoid "Assignment had no effect" from GN.
# It's possible this variable is only used in if conditions (e.g. is_host).
not_needed([ "zbi_sources" ])

fidl("zbi") {
	sources = zbi_sources
}`,
		},
		{
			name: "raw_overwrite on entire rustc_env dictionary attribute",
			bazel: `rustc_library(
	name = "lib",
	rustc_env = {
		"FOO": "bar",
	},  # @bazel2gn:raw_overwrite:[ "FOO=overwritten" ]
)
`,
			wantGN: `rustc_library("lib") {
	rustenv = [ "FOO=overwritten" ]
}`,
		},
		{
			// `raw_overwrite` is ignored because `rustenvToGN()`, which handles the `rustc_env`
			// attribute, only converts keys and values and does not check for `raw_overwrite`
			// annotations on dictionary entries.
			// TODO(https://fxbug.dev/543568916): This should fail or generate
			// `rustenv = [ "FOO=overwritten" ]`.
			name: "unexpected success converting: raw_overwrite on rustc_env dictionary entry",
			bazel: `rustc_library(
	name = "lib",
	rustc_env = {
		"FOO": "bar",  # @bazel2gn:raw_overwrite:"FOO=overwritten"
	}
)
`,
			wantGN: `rustc_library("lib") {
	rustenv = [
		"FOO=bar",
	]
}`,
		},
		{
			// `raw_overwrite` is ignored because target name extraction only converts `binaryExpr.Y`
			// (the RHS literal) and does not check for `raw_overwrite` annotations on the
			// `name = "..."` assignment statement.
			// TODO(https://fxbug.dev/543568916): This should fail or generate
			// `go_library("overwritten_name") { }`.
			name: "unexpected success converting: raw_overwrite on name attribute",
			bazel: `go_library(
	name = "test",  # @bazel2gn:raw_overwrite:"overwritten_name"
)
`,
			wantGN: `go_library("test") {
}`,
		},
		{
			// `raw_overwrite` is ignored because `StmtToGN()`, which converts top-level statements, only
			// checks for `skip` annotations and does not check for `raw_overwrite` annotations.
			// TODO(https://fxbug.dev/543568916): This should fail or generate
			// `group("test_group") { deps = [ ":bar"] }`.
			name: "unexpected success converting: raw_overwrite on target statement",
			bazel: `go_library(
	name = "test",
) # @bazel2gn:raw_overwrite:group("test_group") { deps = [ ":bar"] }
`,
			wantGN: `go_library("test") {
}`,
		},
		{
			name: "raw_overwrite at the end of select() statement",
			bazel: `go_library(
	name = "test",
	deps = select({
		"@platforms//os:fuchsia": [ "//src:foo" ],
		"//conditions:default": [],
	}),  # @bazel2gn:raw_overwrite:[ "//src:overwritten" ]
)
`,
			wantGN: `go_library("test") {
	deps = [ "//src:overwritten" ]
}`,
		},
		{
			// TODO(https://fxbug.dev/543568916): This should fail or overwrite the list elements.
			name: "raw_overwrite inside select() statement - on list item",
			bazel: `go_library(
	name = "test",
	deps = select({
		"@platforms//os:fuchsia": [
			"//src:foo"  # @bazel2gn:raw_overwrite:"//src/overwritten/fuchsia"
		],
		"//conditions:default": [
			"//src:host",  # @bazel2gn:raw_overwrite:"//src/overwritten/host"
		],
	}),
)
`,
			wantGN: `go_library("test") {
	if (is_fuchsia) {
		deps = [
			"//src:foo",
		]
	} else {
		deps = [
			"//src:host",
		]
	}
}`,
		},
		{
			// TODO(https://fxbug.dev/543568916): This should fail or overwrite the list.
			name: "raw_overwrite inside select() statement - on condition value",
			bazel: `go_library(
	name = "test",
	deps = select({
		"@platforms//os:fuchsia": [
			"//src:foo"
		],  # @bazel2gn:raw_overwrite:[ "//src/overwritten/fuchsia" ]
		"//conditions:default": [
			"//src:host",
		],  # @bazel2gn:raw_overwrite:[ "//src/overwritten/host" ]
	}),
)
`,
			wantGN: `go_library("test") {
	if (is_fuchsia) {
		deps = [
			"//src:foo",
		]
	} else {
		deps = [
			"//src:host",
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

// Tests valid annotation scenarios not covered by other tests.
func TestAnnotations(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "skipped attribute ignores unknown annotation on list item",
			bazel: `go_library(
	name = "test",
	visibility = [
		"//path/to/foo:__pkg__",  # @bazel2gn:unknown_annotation
		"//path/to/bar:__subpackages__",
	],  # @bazel2gn:skip
)`,
			wantGN: `go_library("test") {
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

// Tests invalid annotations that should be caught and fail the conversion. However, they are
// currently ignored or processed in unexpected ways.
// TODO(https://fxbug.dev/543568916): Make conversion fail in these cases and change the tests to
// expect errors.
func TestInvalidAnnotations(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			// The skip annotation is ignored in all cases. When `raw_overwrite` comes first, the
			// generated output includes the skip annotation. When that skip annotation does not begin
			// with a second `#`, the generated GN is invalid.
			// TODO(https://fxbug.dev/543568916): This should fail.
			name: "unexpected success converting: multiple annotations on the same list item line",
			bazel: `go_library(
	name = "test",
	visibility = [
		"//path/to/foo:__pkg__",  # @bazel2gn:raw_overwrite:"//foo/*" # @bazel2gn:skip
		"//path/to/bar:bar",  # @bazel2gn:skip # @bazel2gn:raw_overwrite:"//bar/*"
		"//path/to/foo2:__pkg__",  # @bazel2gn:raw_overwrite:"//foo2:*" @bazel2gn:skip
		"//path/to/bar2:bar2",  # @bazel2gn:skip @bazel2gn:raw_overwrite:"//bar2:*"
		"//redundant/path/in/gn:__pkg__",
	],
)`,
			wantGN: `go_library("test") {
	visibility = [
		":*",
		"//foo/*" # @bazel2gn:skip,
		"//path/to/bar:bar",
		"//foo2:*" @bazel2gn:skip,
		"//path/to/bar2:bar2",
		"//redundant/path/in/gn:*",
	]
}`,
		},
		{
			// The skip annotation is ignored in all cases. When `raw_overwrite` comes first, the
			// generated output includes the skip annotation. When that skip annotation does not begin
			// with a second `#`, the generated GN is invalid.
			// TODO(https://fxbug.dev/543568916): This should fail.
			name: "unexpected success converting: multiple annotations on the same attribute",
			bazel: `cc_library(
	name = "test",
	srcs = [
		"bazel.cc",
	],  # @bazel2gn:raw_overwrite:[ "gn.cc" ] @bazel2gn:skip
	hdrs = [
		"bazel.h",
	],  # @bazel2gn:raw_overwrite:[ "gn.h" ] # @bazel2gn:skip
	visibility = [
		"//path/to/foo2:__pkg__",
	],  # @bazel2gn:skip @bazel2gn:raw_overwrite:[ "//gn/path:*" ]
)`,
			wantGN: `static_library("test") {
	sources = [ "gn.cc" ] @bazel2gn:skip
	public = [ "gn.h" ] # @bazel2gn:skip
	visibility = [
		":*",
		"//path/to/foo2:*",
	]
}`,
		},
		{
			// Only the `raw_overwrite` annotation is processed. Because the clear annotation does not
			// begin with a second `#`, the generated GN is invalid.
			// TODO(https://fxbug.dev/543568916): This should fail.
			name: "unexpected success converting: raw_overwrite and configs clearing annotations",
			bazel: `cc_library(
	name = "test",
	copts = [
		"-Wno-implicit-fallthrough",
	],  # @bazel2gn:raw_overwrite:[ "//build/config:overwritten" ] @bazel2gn:clear
)
`,
			wantGN: `static_library("test") {
	configs += [ "//build/config:overwritten" ] @bazel2gn:clear
}`,
		},
		{
			// Neither the `clear` nor `raw_overwrite` annotations are processed.
			// TODO(https://fxbug.dev/543568916): This should fail.
			name: "unexpected success converting: config clearing and raw_overwrite annotations",
			bazel: `cc_library(
	name = "test",
	copts = [
		"-Wno-implicit-fallthrough",
	],  # @bazel2gn:clear @bazel2gn:raw_overwrite:[ "//build/config:overwritten" ]
)
`,
			wantGN: `static_library("test") {
	configs += [
		"//build/config:Wno-implicit-fallthrough",
	]
}`,
		},
		{
			// TODO(https://fxbug.dev/543568916): This should fail because `path_overwrite` is not
			// supported for `visibility`.
			name: "unexpected success converting: unknown annotation on list item",
			bazel: `go_library(
	name = "test",
	visibility = [
		"//path/to/foo:__pkg__",  # @bazel2gn:path_overwrite:"//*"
		"//path/to/bar:skipped",  # @bazel2gn:skip
		"//redundant/path/in/gn:__pkg__",
	],
)`,
			wantGN: `go_library("test") {
	visibility = [
		":*",
		"//path/to/foo:*",
		"//redundant/path/in/gn:*",
	]
}`,
		},
		{
			// TODO(https://fxbug.dev/543568916): This should fail.
			name: "unexpected success converting: unknown annotation on list item",
			bazel: `go_library(
	name = "test",
	visibility = [
		"//path/to/foo:__pkg__",  # @bazel2gn:unknown_annotation
		"//path/to/bar:__subpackages__",
	],
)`,
			wantGN: `go_library("test") {
	visibility = [
		":*",
		"//path/to/foo:*",
		"//path/to/bar/*",
	]
}`,
		},
		{
			// TODO(https://fxbug.dev/543568916): This should fail.
			name: "unexpected success converting: unknown annotation on the list",
			bazel: `go_library(
	name = "test",
	visibility = [
		"//path/to/foo:__pkg__",
	],  # @bazel2gn:unknown_annotation
)`,
			wantGN: `go_library("test") {
	visibility = [
		":*",
		"//path/to/foo:*",
	]
}`,
		},
		{
			// TODO(https://fxbug.dev/543568916): This should probably fail.
			name: "unexpected success converting: incorrect bazel2gn prefix",
			bazel: `go_library(
	name = "test",
	visibility = [
		"//path/to/bar:bar",  # @bzl2gn:skip
	],
)`,
			wantGN: `go_library("test") {
	visibility = [
		":*",
		"//path/to/bar:bar",
	]
}`,
		},
		{
			// TODO(https://fxbug.dev/543568916): This should fail.
			name: "unexpected success converting: space instead of colon",
			bazel: `go_library(
	name = "test",
	visibility = [
		"//path/to/baz:*",  # @bazel2gn skip
	],
)`,
			wantGN: `go_library("test") {
	visibility = [
		":*",
		"//path/to/baz:*",
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

func TestAspectHintsAndExpectIncludes(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "cml_shard and expect_includes transform .shard labels to .shard.cml paths",
			bazel: `cml_shard(
	name = "client.shard",
	includes = [
		":offer.shard",
		":use.shard",
	],
	shard = "client.shard.cml",
)

expect_includes(
	name = "client_includes",
	includes = [
		":client.shard",
		"//sdk/lib/syslog:use.shard",
		"offer.shard.cml",
	],
	deps = [":base_includes"],
)`,
			wantGN: `expect_includes("client_includes") {
	includes = [
		"client.shard.cml",
		"//sdk/lib/syslog/use.shard.cml",
		"offer.shard.cml",
	]
	deps = [
		":base_includes",
	]
}`,
		},
		{
			name: "aspect_hints as only attribute mapping to deps",
			bazel: `fx_cc_library(
	name = "foo",
	srcs = ["foo.cc"],
	aspect_hints = ["//sdk/lib/syslog:client_includes"],
	deps = ["//zircon/system/ulib/zx"],
)`,
			wantGN: `static_library("foo") {
	sources = [
		"foo.cc",
	]
	deps = [
		"//sdk/lib/syslog:client_includes",
	]
	public_deps = [
		"//zircon/system/ulib/zx",
	]
}`,
		},
		{
			name: "aspect_hints combined with implementation_deps on fx_cc_library",
			bazel: `fx_cc_library(
	name = "foo",
	srcs = ["foo.cc"],
	aspect_hints = ["//sdk/lib/syslog:client_includes"],
	implementation_deps = ["//sdk/lib/fdio"],
	deps = ["//zircon/system/ulib/zx"],
)`,
			wantGN: `static_library("foo") {
	sources = [
		"foo.cc",
	]
	deps = [
		"//sdk/lib/fdio",
	]
	deps += [
		"//sdk/lib/syslog:client_includes",
	]
	public_deps = [
		"//zircon/system/ulib/zx",
	]
}`,
		},
		{
			name: "aspect_hints combined with deps on rustc_library",
			bazel: `rustc_library(
	name = "foo",
	srcs = ["src/lib.rs"],
	aspect_hints = select({
		"@platforms//os:fuchsia": ["//sdk/lib/syslog:client_includes"],
		"//conditions:default": [],
	}),
	deps = ["//third_party/rust_crates/vendor:anyhow"],
)`,
			wantGN: `rustc_library("foo") {
	sources = [
		"src/lib.rs",
	]
	deps = [
		"//third_party/rust_crates:anyhow",
	]
	if (is_fuchsia) {
		deps += [
			"//sdk/lib/syslog:client_includes",
		]
	}
}`,
		},
		{
			name: "aspect_hints combined with conditional fuchsia_implementation_deps",
			bazel: `idk_cc_source_library(
	name = "foo",
	aspect_hints = ["//sdk/lib/syslog:client_includes"],
	fuchsia_implementation_deps = [
		"//zircon/system/ulib/zx_impl",
	],
)`,
			wantGN: `sdk_source_set("foo") {
	deps = []
	if (is_fuchsia) {
		deps += [
			"//zircon/system/ulib/zx_impl",
		]
	}
	deps += [
		"//sdk/lib/syslog:client_includes",
	]
}`,
		},
		{
			name: "aspect_hints combined with select deps without default",
			bazel: `rustc_library(
	name = "foo",
	srcs = ["src/lib.rs"],
	aspect_hints = ["//sdk/lib/syslog:client_includes"],
	deps = select({
		"@platforms//os:fuchsia": ["//third_party/rust_crates/vendor:anyhow"],
	}),
)`,
			wantGN: `rustc_library("foo") {
	deps = []
	sources = [
		"src/lib.rs",
	]
	if (is_fuchsia) {
		deps = [
			"//third_party/rust_crates:anyhow",
		]
	}
	deps += [
		"//sdk/lib/syslog:client_includes",
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

func TestRawAttributeAnnotations(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "single raw_attribute on target",
			bazel: `# @bazel2gn:raw_attribute:output_name = "far"
cc_binary(
	name = "bin",
	srcs = ["main.cc"],
)`,
			wantGN: `executable("bin") {
	sources = [
		"main.cc",
	]
	output_name = "far"
}`,
		},
		{
			name: "multiple raw_attribute annotations with preceding comment",
			bazel: `# Regular comment explaining why raw attributes are needed.
# @bazel2gn:raw_attribute:script = "kgsl_strings.rs.py"
# @bazel2gn:raw_attribute:args = [ rebase_path("in.rs", root_build_dir) ]
genrule(
	name = "gen_strings",
	srcs = ["in.rs"],
	outs = ["out.rs"],
	cmd = "python3 $(location :kgsl_strings.rs.py) $< $@",  # @bazel2gn:skip
)`,
			wantGN: `action("gen_strings") {
	sources = [
		"in.rs",
	]
	outputs = [
		"out.rs",
	]
	script = "kgsl_strings.rs.py"
	args = [ rebase_path("in.rs", root_build_dir) ]
}`,
		},
		{
			name: "raw_attribute with target_compatible_with",
			bazel: `load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")

# @bazel2gn:raw_attribute:output_name = "host_far"
cc_binary(
	name = "bin",
	srcs = ["main.cc"],
	target_compatible_with = HOST_CONSTRAINTS,
)`,
			wantGN: `if (is_host) {
	executable("bin") {
		sources = [
			"main.cc",
		]
		output_name = "host_far"
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

func TestRawAttributeAnnotationErrors(t *testing.T) {
	for _, tc := range []struct {
		name  string
		bazel string
	}{
		{
			name: "empty raw_attribute",
			bazel: `# @bazel2gn:raw_attribute:
cc_binary(
	name = "bin",
	srcs = ["main.cc"],
)`,
		},
		{
			name: "raw_attribute not immediately above target",
			bazel: `# @bazel2gn:raw_attribute:output_name = "far"
# Unrelated comment between annotation and target.
cc_binary(
	name = "bin",
	srcs = ["main.cc"],
)`,
		},
		{
			name: "raw_attribute on attribute instead of target",
			bazel: `cc_binary(
	name = "bin",
	srcs = ["main.cc"],  # @bazel2gn:raw_attribute:output_name = "far"
)`,
		},
		{
			name: "raw_attribute on variable assignment",
			bazel: `# @bazel2gn:raw_attribute:output_name = "far"
FOO = ["bar"]`,
		},
		{
			name: "raw_attribute at bottom of target body before closing paren",
			bazel: `cc_binary(
	name = "bin1",
	srcs = ["main.cc"],
	# @bazel2gn:raw_attribute:output_name = "foo"
)

cc_binary(
	name = "bin2",
	srcs = ["other.cc"],
)`,
		},
		{
			name: "raw_attribute inside list attribute",
			bazel: `cc_binary(
	name = "bin",
	srcs = [
		# @bazel2gn:raw_attribute:output_name = "far"
		"main.cc",
	],
)`,
		},
		{
			name: "raw_attribute on rustc_check_cfg_build_flags",
			bazel: `# @bazel2gn:raw_attribute:output_name = "foo"
rustc_check_cfg_build_flags(
	name = "check_cfg",
)`,
		},
		{
			name: "raw_attribute on package",
			bazel: `# @bazel2gn:raw_attribute:output_name = "foo"
package(default_applicable_licenses = ["//:license"])`,
		},
		{
			name: "raw_attribute on no-op rule without GN equivalent",
			bazel: `# @bazel2gn:raw_attribute:output_name = "foo"
exports_files(["foo.txt"])`,
		},
		{
			name: "empty line between raw_attribute and target",
			bazel: `# @bazel2gn:raw_attribute:output_name = "far"

cc_binary(
	name = "bin",
	srcs = ["main.cc"],
)`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			if _, err := bazelToGN(f); err == nil {
				t.Errorf("Unexpected success converting Bazel targets. Bazel source:\n%s", tc.bazel)
			}
		})
	}
}

func TestCmlShardValidation(t *testing.T) {
	for _, tc := range []struct {
		name  string
		bazel string
	}{
		{
			name: "cml_shard name does not end in .shard",
			bazel: `cml_shard(
	name = "client.shard.cml",
	shard = "client.shard.cml",
)`,
		},
		{
			name: "cml_shard missing shard attribute",
			bazel: `cml_shard(
	name = "client.shard",
)`,
		},
		{
			name: "cml_shard shard does not match name + .cml",
			bazel: `cml_shard(
	name = "client.shard",
	shard = "other.shard.cml",
)`,
		},
		{
			name: "expect_includes includes entry does not end in .shard or .cml",
			bazel: `expect_includes(
	name = "client_includes",
	includes = [":invalid_target"],
)`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			if _, err := bazelToGN(f); err == nil {
				t.Fatalf("Expected error converting invalid Bazel target, got nil:\n%s", tc.bazel)
			}
		})
	}
}
