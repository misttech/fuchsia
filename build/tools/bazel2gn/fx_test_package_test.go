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

func TestFxTestPackageFileToGN(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "explicit test package chain",
			bazel: `fx_cc_binary(
	name = "foo_unittests",
	testonly = True,
	srcs = ["foo_unittest.cc"],
	tags = ["manual"],
)

fx_packaged_binary(
	name = "foo_unittests_packaged_bin",
	testonly = True,
	binary = ":foo_unittests",
	binary_name = "foo_unittests",
)

fx_component_manifest(
	name = "foo-tests-manifest",
	testonly = True,
	component_name = "foo-tests",
	manifest = "meta/foo_unittests.cml",
)

fx_test_component(
	name = "foo-tests-component",
	compiled_manifest = ":foo-tests-manifest",
	component_name = "foo-tests",
	deps = [":foo_unittests_packaged_bin"],
)

fx_package(
	name = "foo-tests-package",
	package_name = "foo-tests",
	test_components = [":foo-tests-component"],
)

fx_test(
	name = "foo-tests",
	package = ":foo-tests-package",
)`,
			wantGN: `executable("foo_unittests") {
	testonly = true
	sources = [
		"foo_unittest.cc",
	]
}
fuchsia_component_manifest("foo-tests-manifest") {
	testonly = true
	component_name = "foo-tests"
	manifest = "meta/foo_unittests.cml"
}
fuchsia_test_component("foo-tests-component") {
	cm_label = ":foo-tests-manifest"
	component_name = "foo-tests"
	deps = [
		":foo_unittests",
	]
}
fuchsia_test_package("foo-tests-package") {
	package_name = "foo-tests-gn"
	test_components = [
		":foo-tests-component",
	]
}`,
		},
		{
			name: "test package without package_name or fx_test",
			bazel: `fx_component_manifest(
	name = "manifest",
	manifest = "meta/foo.cml",
)

fx_test_component(
	name = "component",
	compiled_manifest = ":manifest",
)

fx_package(
	name = "foo-tests",
	test_components = [":component"],
	visibility = ["//src:__subpackages__"],
)`,
			wantGN: `fuchsia_component_manifest("manifest") {
	manifest = "meta/foo.cml"
}
fuchsia_test_component("component") {
	cm_label = ":manifest"
}
fuchsia_test_package("foo-tests") {
	package_name = "foo-tests-gn"
	test_components = [
		":component",
	]
	visibility = [
		":*",
		"//src/*",
	]
}`,
		},
		{
			name: "test package with components, subpackages and test specs",
			bazel: `fx_test_component(
	name = "test-component",
	compiled_manifest = ":test-manifest",
)

fx_component_manifest(
	name = "test-manifest",
	manifest = "meta/test.cml",
)

fx_package(
	name = "foo-tests",
	components = ["//src/foo:component"],
	subpackages = ["//src/bar:package"],
	test_components = [
		":test-component",
		# @bazel2gn:skip
		":bazel-only-test-component",
	],
)

fx_test(
	name = "foo-test",
	environments = [
		"//build/testing/environments:aemu_env",
		"//build/testing/environments:nuc11_env",
	],
	max_log_severity = "ERROR",
	package = ":foo-tests",
)`,
			wantGN: `fuchsia_component_manifest("test-manifest") {
	manifest = "meta/test.cml"
}
fuchsia_test_component("test-component") {
	cm_label = ":test-manifest"
}
fuchsia_test_package("foo-tests") {
	package_name = "foo-tests-gn"
	test_components = [
		":test-component",
	]
	deps = [
		"//src/foo:component",
	]
	subpackages = [
		"//src/bar:package",
	]
	test_specs = {
		environments = [
			aemu_env,
			nuc11_env,
		]
		log_settings = {
			max_severity = "ERROR"
		}
	}
}`,
		},
		{
			name: "build_only test",
			bazel: `fx_test_component(
	name = "component",
	compiled_manifest = ":manifest",
)

fx_package(
	name = "foo-tests",
	test_components = [":component"],
)

fx_test(
	name = "foo-test",
	build_only = True,
	package = ":foo-tests",
)`,
			wantGN: `fuchsia_test_component("component") {
	cm_label = ":manifest"
}
fuchsia_test_package("foo-tests") {
	package_name = "foo-tests-gn"
	test_components = [
		":component",
	]
	test_specs = {
		build_only = true
	}
}`,
		},
		{
			name: "fx_test test_type is set on test components",
			bazel: `fx_test_component(
	name = "component",
	compiled_manifest = ":manifest",
)

fx_test_component(
	name = "system-component",
	compiled_manifest = ":manifest",
	test_type = "system",
)

fx_package(
	name = "foo-tests",
	test_components = [
		":component",
		":system-component",
	],
)

fx_test(
	name = "foo-test",
	package = ":foo-tests",
	test_type = "system",
)`,
			wantGN: `fuchsia_test_component("component") {
	cm_label = ":manifest"
	test_type = "system"
}
fuchsia_test_component("system-component") {
	cm_label = ":manifest"
	test_type = "system"
}
fuchsia_test_package("foo-tests") {
	package_name = "foo-tests-gn"
	test_components = [
		":component",
		":system-component",
	]
}`,
		},
		{
			name: "fx_unittest_component test component",
			bazel: `rustc_test(
	name = "foo_test",
	srcs = ["src/lib.rs"],
)

fx_unittest_component(
	name = "foo-test-component",
	binary = ":foo_test",
	component_name = "foo-test",
	visibility = ["//src:__subpackages__"],
)

# Not in a converted test package.
fx_unittest_component(
	name = "other-test-component",
	binary = ":foo_test",
)

fx_package(
	name = "foo-tests",
	test_components = [":foo-test-component"],
)

fx_test(
	name = "foo-test",
	package = ":foo-tests",
	test_type = "system",
)`,
			wantGN: `rustc_test("foo_test") {
	sources = [
		"src/lib.rs",
	]
}
fuchsia_unittest_component("foo-test-component") {
	deps = [
		":foo_test",
	]
	component_name = "foo-test"
	visibility = [
		":*",
		"//src/*",
	]
	test_type = "system"
}
fuchsia_test_package("foo-tests") {
	package_name = "foo-tests-gn"
	test_components = [
		":foo-test-component",
	]
}`,
		},
		{
			name: "fx_package without test_components is ignored",
			bazel: `fx_component_manifest(
	name = "manifest",
	manifest = "meta/foo.cml",
)

fx_component(
	name = "component",
	compiled_manifest = ":manifest",
)

fx_package(
	name = "package",
	components = [":component"],
)`,
			wantGN: `fuchsia_component_manifest("manifest") {
	manifest = "meta/foo.cml"
}
fuchsia_component("component") {
	cm_label = ":manifest"
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

func TestFxTestPackageErrors(t *testing.T) {
	for _, tc := range []struct {
		name    string
		bazel   string
		wantErr string
	}{
		{
			name: "unsupported fx_package attribute",
			bazel: `fx_test_component(
	name = "component",
	compiled_manifest = ":manifest",
)

fx_package(
	name = "foo-tests",
	resources = [":data"],
	test_components = [":component"],
)`,
			wantErr: `attribute "resources"`,
		},
		{
			name: "test_components is not a list literal",
			bazel: `fx_package(
	name = "foo-tests",
	test_components = COMPONENTS,
)`,
			wantErr: "expected list literal for fx_package test_components",
		},
		{
			name: "multiple fx_test targets for one package",
			bazel: `fx_package(
	name = "foo-tests",
	test_components = [":component"],
)

fx_test(
	name = "foo-test",
	package = ":foo-tests",
)

fx_test(
	name = "foo-test-2",
	package = ":foo-tests",
)`,
			wantErr: "is the package of 2 fx_test targets",
		},
		{
			name: "custom fx_test environment",
			bazel: `fx_test_component(
	name = "component",
	compiled_manifest = ":manifest",
)

fx_package(
	name = "foo-tests",
	test_components = [":component"],
)

fx_test(
	name = "foo-test",
	environments = [":custom_env"],
	package = ":foo-tests",
)`,
			wantErr: "only environments in",
		},
		{
			name: "unsupported fx_test attribute",
			bazel: `fx_test_component(
	name = "component",
	compiled_manifest = ":manifest",
)

fx_package(
	name = "foo-tests",
	test_components = [":component"],
)

fx_test(
	name = "foo-test",
	args = ["--foo"],
	package = ":foo-tests",
)`,
			wantErr: `attribute "args" of fx_test`,
		},
		{
			name: "non-boolean fx_test build_only",
			bazel: `fx_test_component(
	name = "component",
	compiled_manifest = ":manifest",
)

fx_package(
	name = "foo-tests",
	test_components = [":component"],
)

fx_test(
	name = "foo-test",
	build_only = "yes",
	package = ":foo-tests",
)`,
			wantErr: "build_only must be True or False",
		},
		{
			name: "non-literal fx_test test_type",
			bazel: `fx_test_component(
	name = "component",
	compiled_manifest = ":manifest",
)

fx_package(
	name = "foo-tests",
	test_components = [":component"],
)

fx_test(
	name = "foo-test",
	package = ":foo-tests",
	test_type = TEST_TYPE,
)`,
			wantErr: `fx_test "foo-test" test_type:`,
		},
		{
			name: "unsupported fx_test_component attribute",
			bazel: `fx_test_component(
	name = "component",
	compiled_manifest = ":manifest",
	moniker = "/core/foo",
)

fx_package(
	name = "foo-tests",
	test_components = [":component"],
)`,
			wantErr: `attribute "moniker"`,
		},
		{
			name: "skipped test component in converted package",
			bazel: `# @bazel2gn:skip
fx_test_component(
	name = "component",
	compiled_manifest = ":manifest",
)

fx_package(
	name = "foo-tests",
	test_components = [":component"],
)`,
			wantErr: "isn't an un-skipped fx_test_component or fx_unittest_component",
		},
		{
			name: "undefined test component in converted package",
			bazel: `fx_package(
	name = "foo-tests",
	test_components = [":component"],
)`,
			wantErr: "isn't an un-skipped fx_test_component or fx_unittest_component",
		},
		{
			name: "unsupported fx_unittest_component attribute",
			bazel: `fx_unittest_component(
	name = "component",
	binary = ":foo_test",
	moniker = "/core/foo",
)

fx_package(
	name = "foo-tests",
	test_components = [":component"],
)`,
			wantErr: `attribute "moniker" of fx_unittest_component`,
		},
		{
			name: "raw_attribute on fx_package",
			bazel: `fx_test_component(
	name = "component",
	compiled_manifest = ":manifest",
)

# @bazel2gn:raw_attribute:testonly = true
fx_package(
	name = "foo-tests",
	test_components = [":component"],
)`,
			wantErr: "raw_attribute annotation is not supported on fx_package",
		},
		{
			name: "raw_attribute on fx_unittest_component",
			bazel: `# @bazel2gn:raw_attribute:testonly = true
fx_unittest_component(
	name = "component",
	binary = ":foo_test",
)

fx_package(
	name = "foo-tests",
	test_components = [":component"],
)`,
			wantErr: "raw_attribute annotation is not supported on fx_unittest_component",
		},
		{
			name: "conflicting test_type",
			bazel: `fx_test_component(
	name = "component",
	compiled_manifest = ":manifest",
	test_type = "starnix",
)

fx_package(
	name = "foo-tests",
	test_components = [":component"],
)

fx_test(
	name = "foo-test",
	package = ":foo-tests",
	test_type = "system",
)`,
			wantErr: `test_type "starnix" conflicts with test_type "system"`,
		},
		{
			name: "conflicting test_type across fx_packages",
			bazel: `fx_test_component(
	name = "component",
	compiled_manifest = ":manifest",
)

fx_package(
	name = "foo-tests",
	test_components = [":component"],
)

fx_test(
	name = "foo-test",
	package = ":foo-tests",
)

fx_package(
	name = "foo-system-tests",
	test_components = [":component"],
)

fx_test(
	name = "foo-system-test",
	package = ":foo-system-tests",
	test_type = "system",
)`,
			wantErr: "in fx_packages whose fx_tests have different test_type",
		},
		{
			name: "fx_test test_type with cross-package test component",
			bazel: `fx_package(
	name = "foo-tests",
	test_components = ["//other/pkg:component"],
)

fx_test(
	name = "foo-test",
	package = ":foo-tests",
	test_type = "system",
)`,
			wantErr: `has test component "//other/pkg:component" from another package`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			_, err := bazel2gn.FileToGN(f)
			if err == nil {
				t.Fatalf("FileToGN() succeeded, want error containing %q", tc.wantErr)
			}
			if !strings.Contains(err.Error(), tc.wantErr) {
				t.Errorf("FileToGN() error = %q, want substring %q", err.Error(), tc.wantErr)
			}
		})
	}
}
