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

func TestFxComponentFileToGN(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "multiple components sharing rustc_binary via fx_packaged_binary",
			bazel: `rustc_binary(
	name = "http_client",
	srcs = ["src/main.rs"],
	edition = "2024",
	deps = [":http_client_config"],
)

fx_packaged_binary(
	name = "http_client_packaged_bin",
	binary = ":http_client",
	binary_name = "http_client",
)

fx_component_manifest(
	name = "manifest",
	component_name = "http-client",
	includes = ["//sdk/lib/syslog:client.shard"],
	manifest = "meta/http_client.cml",
)

fx_component(
	name = "component",
	compiled_manifest = ":manifest",
	component_name = "http-client",
	deps = [":http_client_packaged_bin"],
)

fx_component_manifest(
	name = "google_manifest",
	component_name = "google-http-client",
	includes = ["//sdk/lib/syslog:client.shard"],
	manifest = "//src/connectivity/policy/http-client:meta/google_http_client.cml",
)

fx_component(
	name = "google_component",
	compiled_manifest = ":google_manifest",
	component_name = "google-http-client",
	deps = [":http_client_packaged_bin"],
)

fuchsia_structured_config_rust_lib(
	name = "http_client_config",
	cm_label = ":manifest",
)`,
			wantGN: `rustc_binary("http_client") {
	sources = [
		"src/main.rs",
	]
	edition = "2024"
	deps = [
		":http_client_config",
	]
}
fuchsia_component_manifest("manifest") {
	component_name = "http-client"
	manifest = "meta/http_client.cml"
}
fuchsia_component("component") {
	cm_label = ":manifest"
	component_name = "http-client"
	deps = [
		":http_client",
	]
}
fuchsia_component_manifest("google_manifest") {
	component_name = "google-http-client"
	manifest = "//src/connectivity/policy/http-client/meta/google_http_client.cml"
}
fuchsia_component("google_component") {
	cm_label = ":google_manifest"
	component_name = "google-http-client"
	deps = [
		":http_client",
	]
}
fuchsia_structured_config_rust_lib("http_client_config") {
	cm_label = ":manifest"
}`,
		},
		{
			name: "rustc_binary with crate_name matching binary_name unwraps directly",
			bazel: `rustc_binary(
	name = "exit-with-error-bin",
	testonly = True,
	srcs = ["src/main.rs"],
	crate_name = "exit_with_error_bin",
	edition = "2024",
)

fx_packaged_binary(
	name = "exit-with-error-bin_packaged_bin",
	testonly = True,
	binary = ":exit-with-error-bin",
	binary_name = "exit_with_error_bin",
)

fx_component_manifest(
	name = "component-manifest",
	testonly = True,
	component_name = "exit-with-error",
	includes = ["//sdk/lib/syslog:client.shard"],
	manifest = "meta/exit-with-error.cml",
)

fx_component(
	name = "component",
	testonly = True,
	compiled_manifest = ":component-manifest",
	component_name = "exit-with-error",
	deps = [":exit-with-error-bin_packaged_bin"],
)`,
			wantGN: `rustc_binary("exit-with-error-bin") {
	testonly = true
	sources = [
		"src/main.rs",
	]
	output_name = "exit_with_error_bin"
	edition = "2024"
}
fuchsia_component_manifest("component-manifest") {
	testonly = true
	component_name = "exit-with-error"
	manifest = "meta/exit-with-error.cml"
}
fuchsia_component("component") {
	testonly = true
	cm_label = ":component-manifest"
	component_name = "exit-with-error"
	deps = [
		":exit-with-error-bin",
	]
}`,
		},
		{
			name: "fx_packaged_binary with renamed binary_name or custom install_root emits renamed_binary",
			bazel: `fx_cc_binary(
	name = "my_tool",
	srcs = ["main.cc"],
)

fx_packaged_binary(
	name = "renamed_tool_packaged_bin",
	testonly = True,
	binary = ":my_tool",
	binary_name = "alt_tool",
	install_root = "tools/",
	visibility = ["//visibility:private"],
)

fx_component_manifest(
	name = "manifest",
	testonly = True,
	manifest = "meta/tool.cml",
)

fx_component(
	name = "comp_a",
	testonly = True,
	compiled_manifest = ":manifest",
	deps = [":renamed_tool_packaged_bin"],
)

fx_component(
	name = "comp_b",
	testonly = True,
	compiled_manifest = ":manifest",
	deps = [":renamed_tool_packaged_bin"],
)`,
			wantGN: `executable("my_tool") {
	sources = [
		"main.cc",
	]
}
renamed_binary("renamed_tool_packaged_bin") {
	testonly = true
	source = "$root_out_dir/my_tool"
	source_deps = [
		":my_tool",
	]
	dest = "tools/alt_tool"
	visibility = [
		":*",
	]
}
fuchsia_component_manifest("manifest") {
	testonly = true
	manifest = "meta/tool.cml"
}
fuchsia_component("comp_a") {
	testonly = true
	cm_label = ":manifest"
	deps = [
		":renamed_tool_packaged_bin",
	]
}
fuchsia_component("comp_b") {
	testonly = true
	cm_label = ":manifest"
	deps = [
		":renamed_tool_packaged_bin",
	]
}`,
		},
		{
			name: "hoists fx_component_manifest defined after fx_component",
			bazel: `fx_package(
	name = "empty_container",
	package_name = "starless",
	components = [":empty_container_component"],
)

fx_component(
	name = "empty_container_component",
	compiled_manifest = ":empty_container_manifest",
	component_name = "empty_container",
	deps = ["//src/starnix/containers:default_init"],
)

fx_component_manifest(
	name = "empty_container_manifest",
	component_name = "empty_container",
	includes = ["//sdk/lib/syslog:client.shard"],
	manifest = "meta/empty_container.cml",
)`,
			wantGN: `fuchsia_component_manifest("empty_container_manifest") {
	component_name = "empty_container"
	manifest = "meta/empty_container.cml"
}
fuchsia_component("empty_container_component") {
	cm_label = ":empty_container_manifest"
	component_name = "empty_container"
	deps = [
		"//src/starnix/containers:default_init",
	]
}`,
		},
		{
			name: "unreferenced fx_packaged_binary and skipped fx_component are ignored",
			bazel: `fx_packaged_binary(
	name = "cross_pkg_bin",
	binary = "//src/sys/pkg/bin/far:bin",
	binary_name = "far",
)

# @bazel2gn:skip
fx_component_manifest(
	name = "skipped_manifest",
	manifest = "meta/skipped.cml",
)

# @bazel2gn:skip
fx_component(
	name = "skipped_component",
	compiled_manifest = ":skipped_manifest",
	deps = [":cross_pkg_bin"],
)

fx_test_component(
	name = "test_component",
	compiled_manifest = ":skipped_manifest",
	deps = [":cross_pkg_bin"],
)`,
			wantGN: "",
		},
		{
			name: "resolves fx_packaged_binary inside select and shared variable",
			bazel: `fx_cc_binary(
	name = "bin_a",
	srcs = ["a.cc"],
)

fx_cc_binary(
	name = "bin_b",
	srcs = ["b.cc"],
)

fx_packaged_binary(
	name = "bin_a_pkg",
	binary = ":bin_a",
	binary_name = "bin_a",
)

fx_packaged_binary(
	name = "bin_b_pkg",
	binary = ":bin_b",
	binary_name = "bin_b",
)

_COMMON_DEPS = [":bin_a_pkg"]

fx_component_manifest(
	name = "manifest",
	manifest = "meta/c.cml",
)

fx_component(
	name = "component",
	compiled_manifest = ":manifest",
	deps = _COMMON_DEPS + select({
		"@platforms//os:fuchsia": [":bin_b_pkg"],
		"//conditions:default": [],
	}),
)`,
			wantGN: `executable("bin_a") {
	sources = [
		"a.cc",
	]
}
executable("bin_b") {
	sources = [
		"b.cc",
	]
}
_COMMON_DEPS = [
	":bin_a",
]

# To avoid "Assignment had no effect" from GN.
# It's possible this variable is only used in if conditions (e.g. is_host).
not_needed([ "_COMMON_DEPS" ])

fuchsia_component_manifest("manifest") {
	manifest = "meta/c.cml"
}
fuchsia_component("component") {
	cm_label = ":manifest"
	deps = []
	deps += _COMMON_DEPS
	if (is_fuchsia) {
		deps += [
			":bin_b",
		]
	}
}`,
		},
		{
			name: "auto-skips fx_component_manifest consumed only by fx_test_component or skipped fx_component",
			bazel: `rustc_library(
	name = "lib",
	srcs = ["src/lib.rs"],
	edition = "2024",
)

fx_packaged_binary(
	name = "lib_test_packaged_bin",
	testonly = True,
	binary = ":lib_test",
	binary_name = "lib_lib_test",
)

fx_component_manifest(
	name = "test-manifest",
	testonly = True,
	component_name = "lib-test",
	includes = ["//sdk/lib/syslog:client.shard"],
	manifest = "meta/lib_test.cml",
)

fx_test_component(
	name = "test-component",
	compiled_manifest = ":test-manifest",
	component_name = "lib-test",
	deps = [":lib_test_packaged_bin"],
)

fx_component_manifest(
	name = "skipped-comp-manifest",
	manifest = "meta/skipped.cml",
)

# @bazel2gn:skip
fx_component(
	name = "skipped-comp",
	compiled_manifest = ":skipped-comp-manifest",
)

fx_component_manifest(
	name = "shared-manifest",
	manifest = "meta/shared.cml",
)

# @bazel2gn:skip
fx_component(
	name = "skipped-shared-comp",
	compiled_manifest = ":shared-manifest",
)

fuchsia_structured_config_rust_lib(
	name = "shared_config",
	cm_label = ":shared-manifest",
)`,
			wantGN: `rustc_library("lib") {
	sources = [
		"src/lib.rs",
	]
	edition = "2024"
}
fuchsia_component_manifest("shared-manifest") {
	manifest = "meta/shared.cml"
}
fuchsia_structured_config_rust_lib("shared_config") {
	cm_label = ":shared-manifest"
}`,
		},
		{
			name: "fx_component_manifest with restricted_features",
			bazel: `fx_component_manifest(
	name = "manifest",
	component_name = "dispatcher",
	manifest = "meta/dispatcher.cml",
	restricted_features = ["dynamic_dictionaries"],
)

fx_component(
	name = "component",
	compiled_manifest = ":manifest",
	component_name = "dispatcher",
)`,
			wantGN: `fuchsia_component_manifest("manifest") {
	component_name = "dispatcher"
	manifest = "meta/dispatcher.cml"
	restricted_features = [
		"dynamic_dictionaries",
	]
}
fuchsia_component("component") {
	cm_label = ":manifest"
	component_name = "dispatcher"
}`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			lines, err := bazel2gn.FileToGN(f)
			if err != nil {
				t.Fatalf("FileToGN() failed: %v", err)
			}
			if diff := cmp.Diff(tc.wantGN, strings.Join(lines, "\n")); diff != "" {
				t.Errorf("FileToGN() diff (-want +got):\n%s", diff)
			}
		})
	}
}

func TestFxComponentErrors(t *testing.T) {
	for _, tc := range []struct {
		name  string
		bazel string
	}{
		{
			name: "fx_component_manifest missing manifest",
			bazel: `fx_component_manifest(
	name = "manifest",
)`,
		},
		{
			name: "fx_component_manifest unsupported attribute",
			bazel: `fx_component_manifest(
	name = "manifest",
	manifest = "meta/foo.cml",
	required_offers = ["fuchsia.logger.LogSink"],
)`,
		},
		{
			name: "fx_component missing compiled_manifest",
			bazel: `fx_component(
	name = "component",
)`,
		},
		{
			name: "fx_component cross-package compiled_manifest",
			bazel: `fx_component(
	name = "component",
	compiled_manifest = "//other/pkg:manifest",
)`,
		},
		{
			name: "fx_component unsupported attribute",
			bazel: `fx_component(
	name = "component",
	compiled_manifest = ":manifest",
	data = ["foo.txt"],
)`,
		},
		{
			name: "fx_packaged_binary referencing cross-package binary",
			bazel: `fx_packaged_binary(
	name = "pkg_bin",
	binary = "//other/pkg:bin",
	binary_name = "bin",
)

fx_component_manifest(
	name = "manifest",
	manifest = "meta/foo.cml",
)

fx_component(
	name = "component",
	compiled_manifest = ":manifest",
	deps = [":pkg_bin"],
)`,
		},
		{
			name: "fx_packaged_binary referencing skipped binary",
			bazel: `# @bazel2gn:skip
fx_cc_binary(
	name = "bin",
	srcs = ["main.cc"],
)

fx_packaged_binary(
	name = "pkg_bin",
	binary = ":bin",
	binary_name = "bin",
)

fx_component_manifest(
	name = "manifest",
	manifest = "meta/foo.cml",
)

fx_component(
	name = "component",
	compiled_manifest = ":manifest",
	deps = [":pkg_bin"],
)`,
		},
		{
			name: "skipped fx_packaged_binary requiring renamed_binary",
			bazel: `fx_cc_binary(
	name = "bin",
	srcs = ["main.cc"],
)

# @bazel2gn:skip
fx_packaged_binary(
	name = "pkg_bin",
	binary = ":bin",
	binary_name = "renamed_bin",
)

fx_component_manifest(
	name = "manifest",
	manifest = "meta/foo.cml",
)

fx_component(
	name = "component",
	compiled_manifest = ":manifest",
	deps = [":pkg_bin"],
)`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			if _, err := bazel2gn.FileToGN(f); err == nil {
				t.Fatalf("Expected error converting invalid Bazel file, got nil:\n%s", tc.bazel)
			}
		})
	}
}
