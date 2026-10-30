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

func TestLinuxResourceFileToGN(t *testing.T) {
	for _, tc := range []struct {
		name   string
		bazel  string
		wantGN string
	}{
		{
			name: "hello_starnix fx_cc_binary and linux_resource",
			bazel: `fx_cc_binary(
	name = "hello_starnix_bin_bin",
	srcs = select({
		"@platforms//cpu:x86_64": ["hello_x64.S"],
		"@platforms//cpu:arm64": ["hello_aarch64.S"],
		"@platforms//cpu:riscv64": ["hello_riscv64.S"],
	}),
	disable_build_flags = ["//build/config/linux:implicit-host-libs"],
	disable_instrumentation = True,
	disable_syslog_backend = True,
	linkopts = [
		"-nostdlib",
		"-nostartfiles",
		"-static",
		"-Wno-unused-command-line-argument",
	],
	tags = ["manual"],
)

linux_resource(
	name = "hello_starnix_bin",
	src = ":hello_starnix_bin_bin",
	dest = "data/tests/hello_starnix",
	visibility = ["//src/starnix/kernel/core:__pkg__"],
)

fx_component_manifest(
	name = "manifest",
	component_name = "hello_starnix",
	manifest = "meta/hello_starnix.cml",
)

fx_component(
	name = "component",
	compiled_manifest = ":manifest",
	deps = [":hello_starnix_bin"],
)

fx_package(
	name = "hello_starnix",
	components = [":component"],
)`,
			wantGN: `if (is_linux && !is_host) {
	executable("hello_starnix_bin_bin") {
		if (current_cpu == "x64") {
			sources = [
				"hello_x64.S",
			]
		} else if (current_cpu == "arm64") {
			sources = [
				"hello_aarch64.S",
			]
		} else if (current_cpu == "riscv64") {
			sources = [
				"hello_riscv64.S",
			]
		}
		configs -= [
			"//build/config/linux:implicit-host-libs",
		]
		exclude_toolchain_tags = [ "instrumented" ]
		disable_syslog_backend = true
		ldflags = [
			"-nostdlib",
			"-nostartfiles",
			"-static",
			"-Wno-unused-command-line-argument",
		]
	}
}
linux_resource("hello_starnix_bin") {
	src = ":hello_starnix_bin_bin"
	dest = "data/tests/hello_starnix"
	visibility = [
		":*",
		"//src/starnix/kernel/core:*",
	]
}
fuchsia_component_manifest("manifest") {
	component_name = "hello_starnix"
	manifest = "meta/hello_starnix.cml"
}
fuchsia_component("component") {
	cm_label = ":manifest"
	deps = [
		":hello_starnix_bin",
	]
}`,
		},
		{
			name: "rustc_binary with custom crate_name and linux_resource",
			bazel: `rustc_binary(
	name = "thermal_client_exe",
	testonly = True,
	srcs = ["src/thermal_client.rs"],
	crate_name = "thermal_client",
	crate_root = "src/thermal_client.rs",
	edition = "2024",
	tags = [
		"exclude_toolchain_tags=instrumented",
		"manual",
	],
	deps = ["//third_party/rust_crates/vendor:libc"],
)

linux_resource(
	name = "thermal_client_bin",
	testonly = True,
	src = ":thermal_client_exe",
	dest = "data/tests/thermal_client",
	tags = ["assert_no_deps=//sdk/lib/fdio"],
)`,
			wantGN: `if (is_linux && !is_host) {
	rustc_binary("thermal_client_exe") {
		testonly = true
		sources = [
			"src/thermal_client.rs",
		]
		output_name = "thermal_client"
		source_root = "src/thermal_client.rs"
		edition = "2024"
		exclude_toolchain_tags = [
			"instrumented",
		]
		deps = [
			"//third_party/rust_crates:libc",
		]
	}
}
linux_resource("thermal_client_bin") {
	testonly = true
	src = ":thermal_client_exe"
	bin_output_name = "thermal_client"
	dest = "data/tests/thermal_client"
	assert_no_deps = [
		"//sdk/lib/fdio",
	]
}`,
		},
		{
			name: "cross-package linux_resource",
			bazel: `linux_resource(
	name = "default_init",
	src = "//src/starnix/containers/default_init",
	dest = "data/init",
	visibility = [
		"//src/starnix/containers:__subpackages__",
		"//src/starnix/kernel:__pkg__",
	],
)`,
			wantGN: `linux_resource("default_init") {
	src = "//src/starnix/containers/default_init"
	dest = "data/init"
	visibility = [
		":*",
		"//src/starnix/containers/*",
		"//src/starnix/kernel:*",
	]
}`,
		},
		{
			name: "linkshared fx_cc_binary with arch32_src select",
			bazel: `fx_cc_binary(
	name = "vdso_lib",
	srcs = ["vdso_common.cc"] + select({
		"@platforms//cpu:arm": ["vdso_arm_eabi.cc"],
		"//conditions:default": [],
	}),
	disable_build_flags = ["//build/config/linux:implicit-host-libs"],
	disable_instrumentation = True,
	linkopts = [
		"-Wl,-T,vdso.ld",  # @bazel2gn:raw_overwrite:"-Wl,-T," + rebase_path("vdso.ld", root_build_dir)
	],
	linkshared = True,
	tags = ["manual"],
	target_compatible_with = [
		"//build/bazel/platforms:linux_flavor_starnix",
	],
)

linux_resource(
	name = "vdso_data",
	src = ":vdso_lib",  # @bazel2gn:path_overwrite::vdso_lib($bionic_linux_toolchain)
	dest = "data/libvdso.so",
	arch32_src = select({
		"@platforms//cpu:arm64": ":vdso_lib",
		"//conditions:default": None,
	}),
	arch32_dest = "data/libvdso_arch32.so",
)`,
			wantGN: `if (is_linux && !is_host) {
	loadable_module("vdso_lib") {
		sources = []
		sources += [
			"vdso_common.cc",
		]
		if (current_cpu == "arm") {
			sources += [
				"vdso_arm_eabi.cc",
			]
		}
		configs -= [
			"//build/config/linux:implicit-host-libs",
		]
		exclude_toolchain_tags = [ "instrumented" ]
		ldflags = [
			"-Wl,-T," + rebase_path("vdso.ld", root_build_dir),
		]
	}
}
linux_resource("vdso_data") {
	src = ":vdso_lib($bionic_linux_toolchain)"
	bin_output_name = "vdso_lib.so"
	dest = "data/libvdso.so"
	if (target_cpu == "arm64") {
		arch32_src = ":vdso_lib"
		arch32_bin_output_name = "vdso_lib.so"
		arch32_dest = "data/libvdso_arch32.so"
	}
}`,
		},
		{
			name: "linkshared fx_cc_binary named with .so suffix",
			bazel: `fx_cc_binary(
	name = "libvdso.so",
	srcs = ["vdso.cc"],
	linkshared = True,
)

linux_resource(
	name = "vdso_data",
	src = ":libvdso.so",
	dest = "data/libvdso.so",
)`,
			wantGN: `if (is_linux && !is_host) {
	loadable_module("libvdso.so") {
		output_name = "libvdso"
		sources = [
			"vdso.cc",
		]
	}
}
linux_resource("vdso_data") {
	src = ":libvdso.so"
	dest = "data/libvdso.so"
}`,
		},
		{
			name: "standalone arch32 linux_resource and arm constraint on binary",
			bazel: `fx_cc_binary(
	name = "bin32_bin",
	srcs = ["hello_debian32.S"],
	target_compatible_with = ["@platforms//cpu:arm"],
)

linux_resource(
	name = "bin32",
	arch32_src = ":bin32_bin",
	arch32_dest = "data/tests/hello_debian32_asm",
	target_compatible_with = ["@platforms//cpu:arm64"],
)`,
			wantGN: `if (is_linux && !is_host && current_cpu == "arm") {
	executable("bin32_bin") {
		sources = [
			"hello_debian32.S",
		]
	}
}
if (current_cpu == "arm64") {
	linux_resource("bin32") {
		arch32_src = ":bin32_bin"
		arch32_dest = "data/tests/hello_debian32_asm"
	}
}`,
		},
		{
			name: "binary shared with non-linux_resource consumer is not auto-wrapped in is_linux",
			bazel: `rustc_binary(
	name = "shared_bin",
	srcs = ["src/main.rs"],
	edition = "2024",
	with_unit_tests = True,
)

linux_resource(
	name = "shared_linux_res",
	src = ":shared_bin",
	dest = "data/shared_bin",
)`,
			wantGN: `rustc_binary("shared_bin") {
	sources = [
		"src/main.rs",
	]
	edition = "2024"
	with_unit_tests = true
}
linux_resource("shared_linux_res") {
	src = ":shared_bin"
	dest = "data/shared_bin"
}`,
		},
		{
			name: "same-file helper visibility allowlisting linux binary does not prevent wrapping",
			bazel: `fx_cc_library(
	name = "helper",
	srcs = ["helper.cc"],
	visibility = [":linux_bin"],
)

fx_cc_binary(
	name = "linux_bin",
	srcs = ["main.cc"],
	deps = [":helper"],
)

linux_resource(
	name = "linux_res",
	src = ":linux_bin",
	dest = "data/linux_bin",
)`,
			wantGN: `static_library("helper") {
	sources = [
		"helper.cc",
	]
	visibility = [
		":*",
		":linux_bin",
	]
}
if (is_linux && !is_host) {
	executable("linux_bin") {
		sources = [
			"main.cc",
		]
		deps = [
			":helper",
		]
	}
}
linux_resource("linux_res") {
	src = ":linux_bin"
	dest = "data/linux_bin"
}`,
		},
		{
			name: "fx_cc_test with custom gn_output_name and linux_resource",
			bazel: `fx_cc_test(
	name = "syscall_test_bin",
	testonly = True,
	srcs = ["syscall_test.cc"],
	disable_build_flags = ["//build/config/linux:implicit-host-libs"],
	gn_output_name = "custom_syscall_test",
	deps = ["//third_party/googletest:gtest_main"],
)

linux_resource(
	name = "syscall_test_res",
	testonly = True,
	src = ":syscall_test_bin",
	dest = "data/tests/custom_syscall_test",
)`,
			wantGN: `if (is_linux && !is_host) {
	test("syscall_test_bin") {
		sources = [
			"syscall_test.cc",
		]
		configs -= [
			"//build/config/linux:implicit-host-libs",
		]
		output_name = "custom_syscall_test"
		deps = [
			"//third_party/googletest:gtest_main",
		]
	}
}
linux_resource("syscall_test_res") {
	testonly = true
	src = ":syscall_test_bin"
	bin_output_name = "custom_syscall_test"
	dest = "data/tests/custom_syscall_test"
}`,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			f := toSyntaxFile(t, tc.bazel)
			gotLines, err := bazel2gn.FileToGN(f)
			if err != nil {
				t.Fatalf("Unexpected error converting Bazel file: %v", err)
			}
			gotGN := strings.Join(gotLines, "\n")
			if diff := cmp.Diff(tc.wantGN, gotGN); diff != "" {
				t.Errorf("Diff found afterGN conversion (-want +got):\n%s\nBazel source:\n%s", diff, tc.bazel)
			}
		})
	}
}

func TestLinuxResourceErrors(t *testing.T) {
	for _, tc := range []struct {
		name  string
		bazel string
	}{
		{
			name: "missing name",
			bazel: `linux_resource(
	src = ":bin",
	dest = "data/bin",
)`,
		},
		{
			name: "missing src and arch32_src",
			bazel: `linux_resource(
	name = "res",
)`,
		},
		{
			name: "missing dest when src is set",
			bazel: `linux_resource(
	name = "res",
	src = ":bin",
)`,
		},
		{
			name: "empty dest",
			bazel: `linux_resource(
	name = "res",
	src = ":bin",
	dest = "",
)`,
		},
		{
			name: "leading slash in dest",
			bazel: `linux_resource(
	name = "res",
	src = ":bin",
	dest = "/data/bin",
)`,
		},
		{
			name: "dest without src",
			bazel: `linux_resource(
	name = "res",
	dest = "data/bin",
	arch32_src = ":bin32",
	arch32_dest = "data/bin32",
)`,
		},
		{
			name: "missing arch32_dest when arch32_src is set",
			bazel: `linux_resource(
	name = "res",
	src = ":bin",
	dest = "data/bin",
	arch32_src = select({
		"@platforms//cpu:arm64": ":bin32",
		"//conditions:default": None,
	}),
)`,
		},
		{
			name: "leading slash in arch32_dest",
			bazel: `linux_resource(
	name = "res",
	src = ":bin",
	dest = "data/bin",
	arch32_src = ":bin32",
	arch32_dest = "/data/bin32",
)`,
		},
		{
			name: "arch32_dest without arch32_src",
			bazel: `linux_resource(
	name = "res",
	src = ":bin",
	dest = "data/bin",
	arch32_dest = "data/bin32",
)`,
		},
		{
			name: "unsupported condition in arch32_src select",
			bazel: `linux_resource(
	name = "res",
	src = ":bin",
	dest = "data/bin",
	arch32_src = select({
		"@platforms//cpu:x86_64": ":bin32",
		"//conditions:default": None,
	}),
	arch32_dest = "data/bin32",
)`,
		},
		{
			name: "non-None default in arch32_src select",
			bazel: `linux_resource(
	name = "res",
	src = ":bin",
	dest = "data/bin",
	arch32_src = select({
		"@platforms//cpu:arm64": ":bin32",
		"//conditions:default": ":other_bin",
	}),
	arch32_dest = "data/bin32",
)`,
		},
		{
			name: "unsupported attribute on linux_resource",
			bazel: `linux_resource(
	name = "res",
	src = ":bin",
	dest = "data/bin",
	deps = [":other"],
)`,
		},
		{
			name: "raw_attribute on linux_resource",
			bazel: `# @bazel2gn:raw_attribute:allow_binary_output_dir = true
linux_resource(
	name = "res",
	src = ":bin",
	dest = "data/bin",
)`,
		},
		{
			name: "non-boolean linkshared on fx_cc_binary",
			bazel: `fx_cc_binary(
	name = "lib",
	srcs = ["lib.cc"],
	linkshared = select({
		"@platforms//os:linux": True,
		"//conditions:default": False,
	}),
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
