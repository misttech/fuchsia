// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn

import (
	_ "embed"
	"encoding/json"
	"fmt"
	"maps"
	"regexp"

	"go.starlark.net/syntax"
)

// indentPrefix is the string value used to indent a line by one level.
//
// NOTE: This prefix is only used by the internal representation of this package.
// The final output is formatted by `gn format`.
const indentPrefix = "\t"

// clearAnnotation is a comment annotation that indicates the assignment is an explicit clear.
const clearAnnotation = "# @bazel2gn:clear"

// skipAnnotation is a comment annotation that indicates the statement should be skipped.
const skipAnnotation = "# @bazel2gn:skip"

// noGNEquivalent is the value in bazelRuleToGNTemplate for Bazel rules that
// have no GN counterpart, so bazel2gn skips them instead of failing.
const noGNEquivalent = "__NO_GN_EQUIVALENT__"

// pathOverwriteAnnotationPrefix is a comment annotation prefix that indicates a path should be
// overwritten in GN. In this case bazel2gn will ignore the value set in the BUILD.bazel file
// and use the value specified in the annotation in the BUILD.gn file instead. The value specified
// in the annotation will be wrapped in double quotes.
const pathOverwriteAnnotationPrefix = "# @bazel2gn:path_overwrite:"

// rawOverwriteAnnotationPrefix is a comment annotation prefix that indicates a value should be
// overwritten with a raw GN expression.
const rawOverwriteAnnotationPrefix = "# @bazel2gn:raw_overwrite:"

// rawAttributeAnnotationPrefix is a comment annotation prefix that indicates a
// raw GN attribute or statement should be emitted inside the annotated target.
const rawAttributeAnnotationPrefix = "# @bazel2gn:raw_attribute:"

// valueOverwriteAnnotationPrefix is a comment annotation prefix that indicates a value of a
// dictionary entry should be overwritten in GN. In this case bazel2gn will ignore the value set
// in the BUILD.bazel file and use the value specified in the annotation in the BUILD.gn file
// instead.
const valueOverwriteAnnotationPrefix = "# @bazel2gn:value_overwrite:"

// transformerAnnotationPrefix is a comment annotation prefix that indicates a transformer
// should be applied to the next statement.
const transformerAnnotationPrefix = "# @bazel2gn:transformer="

// transformerAnnotationNames maps from transformer annotation names to the transformer functions.
var transformerAnnotationNames = map[string]transformer{
	"visibility": bazelVisibilityToGN,
	"deps":       bazelDepToGN,
	"configs":    bazelCOptToGNConfig,
	"file_paths": bazelFilePathsToGN,
}

// bazelRuleToGNTemplate maps from Bazel rule names to GN template names. They can
// be the same if Bazel and GN shared the same template name.
//
// This map is also used to check known Bazel rules that can be converted to GN.
// i.e. Bazel rules not found in this map is not supported by bazel2gn yet.
var bazelRuleToGNTemplate = map[string]string{
	// Bazel primitives
	"alias": "group",

	// Go
	"go_binary":  "go_binary",
	"go_library": "go_library",
	"go_test":    "go_test",

	// Python
	"py_library": "python_library",
	"py_binary":  "python_binary",

	// Rust
	"rust_binary":          "rustc_binary",
	"rust_library":         "rustc_library",
	"rustc_binary":         "rustc_binary",
	"rustc_dylib":          "rustc_dylib",
	"rustc_library":        "rustc_library",
	"rustc_proc_macro":     "rustc_macro",
	"rustc_test":           "rustc_test",
	"rust_proc_macro":      "rustc_macro",
	"rust_proto_library":   "rust_proto_library",
	"rustc_bindgen_golden": "rustc_bindgen_golden",

	// C++
	"cc_library":            "static_library",
	"cc_binary":             "executable",
	"fx_cc_binary":          "executable",
	"fx_cc_library_headers": "library_headers",
	"fx_cc_library":         "static_library",
	"fx_cc_test":            "test",

	// C++ Zircon
	"cc_shared_library_zx": "zx_library", // With `sdk="shared"` and `sdk_publishable` not specified.
	"cc_source_library_zx": "zx_library", // With `sdk="source"` and `sdk_publishable` not specified.
	"cc_static_library_zx": "zx_library", // With `sdk="static"` and `sdk_publishable` not specified.

	// FIDL
	"fidl_library":        "fidl",
	"measure_tape":        "measure_tape",
	"zither_fidl_library": "fidl",

	// Components
	"expect_includes":                    "expect_includes",
	"fuchsia_structured_config_rust_lib": "fuchsia_structured_config_rust_lib",
	"fx_component":                       "fuchsia_component",
	"fx_component_manifest":              "fuchsia_component_manifest",
	"fx_compiled_manifest":               "cm",

	// Host tools
	"cc_binary_host_tool": "executable",
	"ffx_plugin":          "ffx_plugin",
	"go_binary_host_tool": "go_binary",

	// IDK
	"idk_cc_shared_library":      "sdk_shared_library",
	"idk_cc_shared_library_zx":   "zx_library", // With `sdk="shared"` and `sdk_publishable = "partner"`.
	"idk_cc_source_library":      "sdk_source_set",
	"idk_cc_source_library_zx":   "zx_library", // With `sdk="source"` and `sdk_publishable = "partner"`.
	"idk_cc_static_library":      "sdk_static_library",
	"idk_cc_static_library_zx":   "zx_library", // With `sdk="static"` and `sdk_publishable = "partner"`.
	"idk_cc_binary_host_tool":    "sdk_executable_host_tool",
	"idk_go_binary_host_tool":    "sdk_go_binary_host_tool",
	"idk_rustc_binary_host_tool": "sdk_rustc_binary_host_tool",

	// Other
	"compiled_action":     "compiled_action",
	"fidlgentest_go_test": "fidlgentest_go_test",
	"genrule":             "action",
	"linux_resource":      "linux_resource",
	"metrics_registry":    "metrics_registry",
	"package":             "package",
	"test_suite":          "group",
	"stamp_group":         "group",

	// `exports_files()` is a concept specific to Bazel, so there is no need to convert it.
	"exports_files": noGNEquivalent,

	// Bazel-only Fuchsia component, package, and test rules. Any GN wrapper
	// targets (such as `bazel_fuchsia_package()` or `bazel_test_suite()`) are
	// maintained by hand above the bazel2gn sentinel in BUILD.gn.
	"cml_shard":             noGNEquivalent,
	"fx_package":            noGNEquivalent,
	"fx_packaged_binary":    noGNEquivalent,
	"fx_runner_shard":       noGNEquivalent,
	"fx_test":               noGNEquivalent,
	"fx_test_component":     noGNEquivalent,
	"fx_unittest_component": noGNEquivalent,

	// Tests of a library run in both build systems while the library has
	// users in both, so these test rules get a GN copy. See
	// https://fuchsia.dev/fuchsia-src/development/build/bazel_concepts/tests#tests-in-both-build-systems
	//
	// The GN copy of an `fx_unittest_package()` gets a different package name,
	// see `fxUnittestPackageToGN()`.
	"fx_unittest_package": "fuchsia_unittest_package",
	"host_go_test":        "go_test",
	"host_rustc_test":     "rustc_test",

	// `wrap_host_rust_test()` wraps an existing `rustc_test()`, which bazel2gn
	// translates to a GN `rustc_test()` that already runs as a host test.
	"wrap_host_rust_test": noGNEquivalent,

	// TODO(https://fxbug.dev/574121341): Translate these host test rules too.
	// Until then, keep hand-written GN copies of these tests above the
	// sentinel.
	"host_cc_test":   noGNEquivalent,
	"host_py_test":   noGNEquivalent,
	"host_shac_test": noGNEquivalent,
	"host_test":      noGNEquivalent,

	// Runtime data for Bazel host tests, so it's only consumed by the rules
	// above.
	"host_test_data_files": noGNEquivalent,

	// `ffx_tool` and `idk_ffx_tool` targets are Bazel-only and reach GN via
	// handwritten `bazel_ffx_tool()` / `bazel_host_tool()` wrappers when needed.
	"ffx_tool":     noGNEquivalent,
	"idk_ffx_tool": noGNEquivalent,
}

// attrsToOmitByRules stores a mapping from known Bazel rules to attributes to
// omit when converting them to GN.
var attrsToOmitByRules = map[string]map[string]bool{
	"go_library": {
		// In GN we default cgo to true when compiling Go code, and explicitly disable
		// it in very few places. However, in Bazel, cgo defaults to false, and
		// require users to explicitly set when C sources are included.
		"cgo": true,

		// go_library.gni doesn't allow overwriting `importpath`, and forces it to be the same as the
		// directory path from FUCHSIA_DIR. This limits our ability to define multiple go_libraries in
		// the same BUILD file. go_library.gni gets around this with `source_dir`, which does not
		// exist in Bazel. So we omit syncing this attribute and let GN infer it, which handles nested
		// directories correctly. In GN we omit `importpath` for almost all non-third-party go_libraries
		// anyways.
		//
		// This is safe to do because incorrectly configured importpath would cause Go compilation to
		// fail, so we will be alerted when importpaths are wrong in Bazel.
		"importpath": true,
	},
	"py_library": {
		// `imports` attribute is a Bazel-only concept. Our GN python_library.gni template
		// handles import paths differently than Bazel.
		"imports": true,
	},
	"genrule": {
		// bazel2gn ignores the `tools` attribute of genrule, and tries to parse it out of the `cmd`
		// attribute. It is the caller's responsibility to ensure that the `cmd` attribute contains
		// the correct tools. Also note since `genrule` is converted to `action` in GN, the `cmd`
		// attribute is converted to `script` and `args` in GN, so only one `tool` is supported.
		"tools": true,
	},
	// TODO(https://fxbug.dev/457605523): Support `includes` conversion to `configs` in GN.
	// Currently the only use case is to set `includes = ["../.."]`, which is covered by
	// `"//build/config:default_include_dirs"` in GN.
	"cc_library":               {"includes": true},
	"fx_cc_library":            {"includes": true},
	"idk_cc_source_library":    {"includes": true},
	"idk_cc_shared_library_zx": {"version_script": true},
	// We do not need to include the "stamp" destination in GN because it is implicitly created by
	// the rule.
	"stamp_group": {"stamp": true},
	// The Bazel test target attribute `size` is not supported in GN.
	"go_test":    {"size": true},
	"rustc_test": {"size": true},
	// `proto_deps` lists imported .proto files so the Bazel code generator can run
	// sandboxed. The GN rust_proto_library() template discovers imports through a
	// depfile instead.
	"rust_proto_library": {"proto_deps": true},
	// `binary_name` names the inner Bazel test binary that the host test
	// wraps. GN has no such wrapper.
	"host_go_test":    {"size": true, "binary_name": true},
	"host_rustc_test": {"size": true, "binary_name": true},
	// `includes` lists `.cml` shard targets and files needed in the Bazel
	// action sandbox. GN runs `cmc` with global include paths and a depfile
	// instead, and `fuchsia_component_manifest()` does not accept `includes`.
	"fx_component_manifest": {"includes": true},
	"fx_compiled_manifest":  {"includes": true},
	// GN `test()` hardcodes `testonly = true` on its inner `cc_test_executable()`
	// without reading `invoker.testonly`, so emitting `testonly` fails `gn gen`
	// with "Assignment had no effect". (`callExprToGN()` rejects non-`True` values.)
	"fx_cc_test": {"testonly": true},
}

// unsupportedAttrsByRules lists attributes that bazel2gn can't translate yet.
// Dropping them would give the GN copy of a test different behavior, so
// bazel2gn fails instead and the test needs `# @bazel2gn:skip`.
var unsupportedAttrsByRules = map[string]map[string]bool{
	"host_go_test":    {"data": true, "test_data": true},
	"host_rustc_test": {"test_data": true, "test_label": true},
}

// implicitConditionsByRules maps Bazel rules that are only compatible with
// some platforms, without setting `target_compatible_with` explicitly, to the
// GN condition that the generated target is wrapped in.
var implicitConditionsByRules = map[string]string{
	// GN's `go_test()` asserts that `args` and `timeout` are only set on host.
	"host_go_test":    "is_host",
	"host_rustc_test": "is_host",
}

// Common Bazel attributes that use different names in GN.
var commonAttrMap = map[string]string{
	"aspect_hints": "deps",
	"srcs":         "sources",
	"hdrs":         "public",
}

var aliasAttrMap = map[string]string{
	"actual": "public_deps",
}

// ccCommonAttrMap maps from attribute names common in Bazel CC rules to GN parameter names.
// This map only includes attributes that have different names in Bazel and GN.
var ccCommonAttrMap = map[string]string{
	"copts": "configs", // Strings are converted to configs by `bazelCOptToGNConfig()`.
}

// ccBinAttrMap maps from attribute names in Bazel CC binary rules to GN parameter names.
// Unlike `cc_library` (where Bazel's `linkopts` propagates publicly to dependents while GN's
// `ldflags` does not), binary targets have no downstream linkers, so `linkopts` maps directly to
// `ldflags`.
var ccBinAttrMap = mustMergeMaps(ccCommonAttrMap, map[string]string{
	"linkopts": "ldflags",
})

// ccLibAttrMap maps from attribute names in Bazel CC library rules to GN parameter names.
// This map only includes attributes that have different names in Bazel and GN.
var ccLibAttrMap = mustMergeMaps(ccCommonAttrMap, map[string]string{
	"deps":                "public_deps",
	"implementation_deps": "deps",
})

// buildFlagsAttrMap maps the `build_flags()` attributes of the `fx_cc_*()`
// macros to GN parameter names. Bazel `build_flags()` targets use the same labels
// as their GN `config()` counterparts, so the labels are kept as-is.
//
// `build_flags` is converted to `configs +=` and `disable_build_flags` to
// `configs -=`, see `bazelAttrToGNAssignmentOps`.
var buildFlagsAttrMap = map[string]string{
	"build_flags":         "configs",
	"disable_build_flags": "configs",
}

// fxCcBinaryAttrMap maps from attribute names in `fx_cc_binary()` to GN parameter names.
// This map only includes attributes that have different names in Bazel and GN.
var fxCcBinaryAttrMap = mustMergeMaps(ccBinAttrMap, buildFlagsAttrMap)

// fxCcLibAttrMap maps from attribute names in `fx_cc_library()` to GN parameter names.
// This map only includes attributes that have different names in Bazel and GN.
var fxCcLibAttrMap = mustMergeMaps(ccLibAttrMap, buildFlagsAttrMap)

// rustCommonAttrMap maps from attribute names common in Bazel Rust rules to GN parameter names.
// This map only includes attributes that have different names in Bazel and GN.
var rustCommonAttrMap = map[string]string{
	"compile_data":         "inputs",
	"data":                 "non_rust_deps",
	"crate_features":       "features",
	"with_host_unit_tests": "with_unit_tests",
	"rustc_flags":          "rustflags",
	"crate_root":           "source_root",
	"rustc_env":            "rustenv",
	// The `build_flags` attribute on `rustc_*()` targets lists `build_flags()`
	// target labels, which use the same labels as their corresponding GN
	// `config()` targets (converted to `configs += [...]` in GN; note that
	// `build_flags()` definitions themselves are hand-ported rather than
	// converted by bazel2gn).
	"build_flags": "configs",
}

// rustBinAttrMap maps from attribute name in Bazel Rust binary and test rules to GN parameter names.
// This map only includes attributes that have different names in Bazel and GN.
var rustBinAttrMap = mustMergeMaps(rustCommonAttrMap, map[string]string{
	"binary_name": "output_name",
	"crate_name":  "output_name",
})

// ffxPluginAttrMap maps from attribute names in Bazel `ffx_plugin()` to GN parameter names.
// This map only includes attributes that have different names in Bazel and GN.
var ffxPluginAttrMap = mustMergeMaps(rustCommonAttrMap, map[string]string{
	"args_srcs":       "args_sources",
	"args_crate_root": "args_source_root",
})

// hostTestAttrMap maps from attribute names in Bazel host test rules to GN
// parameter names.
var hostTestAttrMap = map[string]string{
	"test_args": "args",
}

// hostRustTestAttrMap maps from attribute names in `host_rustc_test()` to GN
// `rustc_test()` parameter names.
var hostRustTestAttrMap = mustMergeMaps(rustBinAttrMap, hostTestAttrMap)

// fidlAttrMap maps from attribute names in Bazel FIDL rules to GN parameter names.
// This map only includes attributes that have different names in Bazel and GN.
var fidlAttrMap = map[string]string{
	"category":     "sdk_category",
	"deps":         "public_deps",
	"library_name": "name",
}

// idkAttrMap maps from attribute name in Bazel IDK rules to GN parameter names.
// This map only includes attributes that have different names in Bazel and GN.
var idkAttrMap = map[string]string{
	"api_area":      "sdk_area",
	"api_file_path": "api",
	"idk_name":      "sdk_name",

	// This renames the variables, but they must be made a `+=` inside a
	// conditional block in `attrAssignmentToGN()`. The default
	// platform-independent variable must be specified before these variables
	// in the `BUILD.bazel` file. This may require using
	// `# buildifier: leave-alone` above the macro.
	"fuchsia_deps":                "public_deps",
	"fuchsia_implementation_deps": "deps",
	"non_fuchsia_deps":            "public_deps",

	// These are not identical because files in `hdrs_for_internal_use` need
	// to be added to GN's `public` as well. This takes care of populating
	// `sdk_headers_for_internal_use`, and `attrAssignmentToGN()` adds the files
	// to `public`.
	"hdrs_for_internal_use": "sdk_headers_for_internal_use",
}

// Lists of attribute names that are specific to Fuchsia and non-Fuchsia builds.
// These are used to add appropriate conditions in the GN file.
var idkFuchsiaSpecificAttrs = map[string]bool{
	"fuchsia_deps":                true,
	"fuchsia_implementation_deps": true,
}
var idkNonFuchsiaSpecificAttrs = map[string]bool{
	"non_fuchsia_deps": true,
}

// Maps from attribute name in Bazel IDK rules to GN `zx_library()` parameter
// names for attributes unique to `zx_library()` instances that are in the IDK.
// Use `idkZxAttrMap` instead.
// This map only includes attributes that have different names in Bazel and GN.
var zxInIDKAttrMap = map[string]string{
	"category": "sdk_publishable",
}

// genruleAttrMap maps from attribute name in Bazel genrule to GN parameter names.
var genruleAttrMap = map[string]string{
	"outs": "outputs",
}

// compiledActionAttrMap maps from attribute names in Bazel `compiled_action()`
// to GN `compiled_action()` parameter names.
//
// `tool` and `args` keep their names, but `args` is converted by
// `compiledActionArgsToGN()`, and `outs` are placed in `$target_gen_dir` by
// `compiledActionOutsTransformer()`.
var compiledActionAttrMap = map[string]string{
	"outs": "outputs",
}

// idkCcAttrMap maps from attribute name in Bazel IDK C++ rules to GN parameter names.
var idkCcAttrMap = mustMergeMaps(idkAttrMap, ccLibAttrMap)

// idkZxAttrMap maps from attribute name in Bazel IDK C++ ZX rules to GN parameter names.
var idkZxAttrMap = mustMergeMaps(idkCcAttrMap, zxInIDKAttrMap)

// idkFIDLAttrMap maps from attribute name in Bazel IDK FIDL rules to GN parameter names.
var idkFIDLAttrMap = mustMergeMaps(idkAttrMap, fidlAttrMap)

// idkCcBinHostToolAttrMap maps from attribute names in Bazel IDK C++ host binary rules to GN parameter names.
var idkCcBinHostToolAttrMap = mustMergeMaps(idkAttrMap, ccBinAttrMap)

// idkRustBinAttrMap maps from attribute name in Bazel IDK Rust binary rules to GN parameter names.
var idkRustBinAttrMap = mustMergeMaps(idkAttrMap, rustBinAttrMap)

// structuredConfigRustLibAttrMap maps from attribute names in Bazel
// `fuchsia_structured_config_rust_lib()` to GN parameter names.
// This map only includes attributes that have different names in Bazel and GN.
var structuredConfigRustLibAttrMap = map[string]string{
	// `name` is the target name in Bazel, so the crate name uses a different
	// attribute.
	"crate_name": "name",
}

// fxComponentAttrMap maps from attribute names in Bazel `fx_component()` to GN
// `fuchsia_component()` parameter names.
var fxComponentAttrMap = map[string]string{
	"compiled_manifest": "cm_label",
}

// pythonBinAttrMap maps from attribute name in Bazel Python binary rules to GN parameter names.
var pythonBinAttrMap = map[string]string{
	"main": "main_source",
}

// A mapping from Bazel rule names to attribute mappings.
// Attribute mappings map from Bazel rule attributes that use different names in GN.
var attrMapsByRules = map[string]map[string]string{
	// Bazel
	"alias": aliasAttrMap,

	// Python
	"py_binary": pythonBinAttrMap,

	// C++
	"cc_library":          ccLibAttrMap,
	"cc_binary":           ccBinAttrMap,
	"cc_binary_host_tool": ccBinAttrMap,
	"fx_cc_binary":        fxCcBinaryAttrMap,
	"fx_cc_library":       fxCcLibAttrMap,
	"fx_cc_test":          fxCcBinaryAttrMap,

	// C++ Zircon
	"cc_shared_library_zx": ccLibAttrMap,
	"cc_source_library_zx": ccLibAttrMap,
	"cc_static_library_zx": ccLibAttrMap,

	// Rust
	"rust_binary":      rustBinAttrMap,
	"rust_library":     rustCommonAttrMap,
	"rust_proc_macro":  rustCommonAttrMap,
	"rustc_binary":     rustBinAttrMap,
	"rustc_dylib":      rustCommonAttrMap,
	"rustc_library":    rustCommonAttrMap,
	"rustc_proc_macro": rustCommonAttrMap,
	"rustc_test":       rustBinAttrMap,
	"host_rustc_test":  hostRustTestAttrMap,

	// Go
	"host_go_test": hostTestAttrMap,

	// FIDL
	"fidl_library":        idkFIDLAttrMap,
	"zither_fidl_library": fidlAttrMap,

	// IDK
	"idk_cc_shared_library":      idkCcAttrMap,
	"idk_cc_shared_library_zx":   idkZxAttrMap,
	"idk_cc_source_library":      idkCcAttrMap,
	"idk_cc_source_library_zx":   idkZxAttrMap,
	"idk_cc_static_library":      idkCcAttrMap,
	"idk_cc_static_library_zx":   idkZxAttrMap,
	"idk_cc_binary_host_tool":    idkCcBinHostToolAttrMap,
	"idk_go_binary_host_tool":    idkAttrMap,
	"idk_rustc_binary_host_tool": idkRustBinAttrMap,

	// Components
	"fuchsia_structured_config_rust_lib": structuredConfigRustLibAttrMap,
	"fx_component":                       fxComponentAttrMap,
	internalFxTestComponentRule:          fxComponentAttrMap,

	// Tools
	"ffx_plugin": ffxPluginAttrMap,

	// Others
	"compiled_action": compiledActionAttrMap,
	"genrule":         genruleAttrMap,
	"metrics_registry": {
		"global_registry": "global",
	},
	"test_suite": {
		"tests": "deps",
	},
}

var extraGnExpressionByRules = map[string]string{
	"cc_shared_library_zx":     `sdk = "shared"`,
	"cc_source_library_zx":     `sdk = "source"`,
	"cc_static_library_zx":     `sdk = "static"`,
	"idk_cc_shared_library_zx": `sdk = "shared"`,
	"idk_cc_source_library_zx": `sdk = "source"`,
	"idk_cc_static_library_zx": `sdk = "static"`,
	"test_suite":               `testonly = true`,
}

// These identifiers with the same meanings are represented differently in Bazel
// and GN. specialIdentifiers maps from their Bazel representations to GN
// representations.
var specialIdentifiers = map[string]string{
	"True":  "true",
	"False": "false",
}

// specialTokens maps from special tokens in Bazel to their GN equivalents.
var specialTokens = map[syntax.Token]string{
	syntax.AND: "&&",
	syntax.OR:  "||",
}

// The following map from Bazel constraints to GN conditions.
//
// The first contains single constraints that appear in a list.
// The second contains variables representing an entire list.
//
// Only add new values when necessary as there are often more appropriate ways
// to express the same logic. For example, "HOST_CONSTRAINTS" is more
// appropriate than "@platforms//os:linux" in most cases. Also,
// "//build/bazel/platforms:fuchsia_platform_x64" may be more appropriate
// than using a list of "@platforms//os:fuchsia" and "@platforms//cpu:x86_64".
var bazelConstraintsToGNConditions = map[string]string{
	"@platforms//os:fuchsia":                       "is_fuchsia",
	"//build/bazel/platforms:linux_flavor_starnix": "is_linux && !is_host",
	"@platforms//cpu:arm":                          `current_cpu == "arm"`,
	"@platforms//cpu:armv7":                        `current_cpu == "arm"`,
	"@platforms//cpu:arm64":                        `current_cpu == "arm64"`,
	"@platforms//cpu:aarch64":                      `current_cpu == "arm64"`,
	"@platforms//cpu:riscv64":                      `current_cpu == "riscv64"`,
	"@platforms//cpu:x86_64":                       `current_cpu == "x64"`,
}
var bazelConstraintListVarsToGNConditions = map[string]string{
	"HOST_CONSTRAINTS":    "is_host",
	"HOST_OS_CONSTRAINTS": "is_host",
}

// thirdPartyRustCrateVendoredRE matches Bazel third-party Rust crate dependency prefixes for purely
// vendored crates. It is used to extract the crate name from the dependency path.
var thirdPartyRustCrateVendoredRE = regexp.MustCompile(`^"\/\/third_party\/rust_crates\/vendor:`)

// thirdPartyRustCrateModifiedRE matches Bazel third-party Rust crate dependency prefixes for
// modified crates. It is used to extract the directory name from the dependency path.
var thirdPartyRustCrateModifiedRE = regexp.MustCompile(`^"\/\/third_party\/rust_crates\/.+\/([a-zA-Z0-9_\.-]+):?[^"]*`)

//go:embed third_party_target_map.json
var thirdPartyTargetMapJSON []byte

// thirdPartyBazelRepos maps from Bazel third-party repository and target labels to their GN equivalent
// dependency paths. The key is the Bazel label, and the value is the GN dependency path.
var thirdPartyBazelRepos = func() map[string]string {
	var m map[string]string
	if err := json.Unmarshal(thirdPartyTargetMapJSON, &m); err != nil {
		panic(fmt.Sprintf("Failed to unmarshal third_party_target_map.json: %v", err))
	}
	return m
}()

// coptToConfig maps from Bazel copt values to configs to use in GN.
var coptToConfig = map[string]string{
	"-Wno-implicit-fallthrough": "//build/config:Wno-implicit-fallthrough",
	"-Wno-vla-cxx-extension":    "//build/config:Wno-vla-cxx-extension",
	"-Wno-conversion":           "//build/config:Wno-conversion",

	// The following are GN `configs` rather than `copt` values. These must be
	// allowed because they both appear as `configs` by the time this map is used.
	// TODO(https://fxbug.dev/421888626): Properly specify the relevant flags in
	// Bazel and convert those to GN configs.
	":ring-config": ":ring-config",
	"//build/config/fuchsia:no_cpp_standard_library": "//build/config/fuchsia:no_cpp_standard_library",
	"//build/config:all_source":                      "//build/config:all_source",

	// The following are Bazel build_flags() targets, which have GN config()
	// equivalents with the same label.
	"//build/config/rust:bootfs": "//build/config/rust:bootfs",
}

// bazelAttrToGNAssignmentOps maps from Bazel attribute names to the assignment operators to use in GN.
//
// NOTE: Entries in this map should be clearly documented.
var bazelAttrToGNAssignmentOps = map[string]string{
	// `configs` in GN are rarely (never?) empty lists because we set them in BUILDCONFIG.gn.
	// Trying to overwrite a non-empty list in GN with a non-empty value will fail.
	// Simply replacing assignment with `+=` works for the initial use cases we need.
	// More complex mechanism may be required if we need to selectively overwrite config assignments.
	//
	// The same applies to all Bazel attributes that are converted to GN `configs`.
	"configs":     "+=", // Passed through some macros. Deprecated.
	"copts":       "+=", // Mapped to "configs" by `bazelCOptToGNConfig`. Deprecated.
	"build_flags": "+=", // Mapped to "configs" with the same labels.
	// `disable_build_flags` removes `build_flags()` from the defaults, which is
	// the equivalent of removing `config()`s from `configs` in GN.
	"disable_build_flags": "-=", // Mapped to "configs" with the same labels.
}

// disableInstrumentationAttr is the name of the `fx_cc_binary()` attribute that
// prevents building the target with instrumented (e.g. sanitizer) variants.
const disableInstrumentationAttr = "disable_instrumentation"

// disableInstrumentationGN is the GN statement equivalent to
// `disable_instrumentation = True`.
const disableInstrumentationGN = `exclude_toolchain_tags = [ "instrumented" ]`

// execPropertiesAttr is the name of the common Bazel attribute setting
// execution properties (e.g. the RBE worker pool) of the actions of a target.
// It is never emitted verbatim in GN, see `execPropertiesToGN()`.
const execPropertiesAttr = "exec_properties"

// rbeUseLargeWorker is the name of the `exec_properties` value defined in
// //build/bazel/rbe:rbe.bzl that runs the actions of a target on large RBE
// workers.
const rbeUseLargeWorker = "RBE_USE_LARGE_WORKER"

// rustHighmemGN are the GN statements equivalent to
// `exec_properties = RBE_USE_LARGE_WORKER` on Rust targets: a large RBE worker
// when Rust RBE is enabled, or the local high memory pool otherwise. See
// //build/toolchain/concurrent_jobs.gni, which is imported by
// bazel_migration.gni.
//
// GN doesn't support nested scope access such as `a.b.c` in one expression,
// so the scope is assigned to a variable first.
var rustHighmemGN = []string{
	"_concurrent_jobs = concurrent_jobs.rust_highmem",
	"configs += _concurrent_jobs.configs",
	`forward_variables_from(_concurrent_jobs.vars, "*")`,
}

// rustHighmemGNTemplates are the GN templates that accept the `configs` and
// `pool` set by `rustHighmemGN`.
var rustHighmemGNTemplates = map[string]bool{
	"rustc_binary":  true,
	"rustc_library": true,
	"rustc_test":    true,
}

// goThirdPartyAggregateDeps lists known third-party GN targets that include sources of multiple Go
// libraries in a single target. These deps need to be handled specially because in Bazel we have a
// 1:1 mapping between go_library targets and Go libraries.
var goThirdPartyAggregateDeps = []string{
	"//third_party/golibs:golang.org/x/crypto",
	"//third_party/golibs:gonum.org/v1/gonum",
	"//third_party/golibs:google.golang.org/protobuf",
}

// mustMergeMaps merges two input maps and return a new one with keys and values from
// both inputs.
//
// This function panics if m1 and m2 have duplicate keys. All these cases should be
// caught at build time so it's OK to panic here.
func mustMergeMaps(m1, m2 map[string]string) map[string]string {
	var m = maps.Clone(m1)
	for k, v := range m2 {
		if _, ok := m[k]; ok {
			panic(fmt.Sprintf("Duplicate key when merging maps: %q", k))
		}
		m[k] = v
	}
	return m
}
