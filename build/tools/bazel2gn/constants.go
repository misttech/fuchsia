// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn

import (
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

// pathOverwriteAnnotationPrefix is a comment annotation prefix that indicates a path should be
// overwritten in GN. In this case bazel2gn will ignore the value set in the BUILD.bazel file
// and use the value specified in the annotation in the BUILD.gn file instead.
const pathOverwriteAnnotationPrefix = "# @bazel2gn:path_overwrite:"

// bazelRuleToGNTemplate maps from Bazel rule names to GN template names. They can
// be the same if Bazel and GN shared the same template name.
//
// This map is also used to check known Bazel rules that can be converted to GN.
// i.e. Bazel rules not found in this map is not supported by bazel2gn yet.
var bazelRuleToGNTemplate = map[string]string{
	// Go
	"go_binary":  "go_binary",
	"go_library": "go_library",
	"go_test":    "go_test",

	// Rust
	"rust_binary":     "rustc_binary",
	"rust_library":    "rustc_library",
	"rustc_binary":    "rustc_binary",
	"rustc_library":   "rustc_library",
	"rust_proc_macro": "rustc_macro",

	// C++
	"cc_library": "source_set",
	"cc_binary":  "executable",

	// C++ Zircon
	"cc_shared_library_zx": "zx_library", // With `sdk="shared"` and `sdk_publishable` not specified.
	"cc_source_library_zx": "zx_library", // With `sdk="source"` and `sdk_publishable` not specified.
	"cc_static_library_zx": "zx_library", // With `sdk="static"` and `sdk_publishable` not specified.

	// FIDL
	"fidl_library":        "fidl",
	"zither_fidl_library": "fidl",

	// IDK
	"idk_cc_shared_library":    "sdk_shared_library",
	"idk_cc_shared_library_zx": "zx_library", // With `sdk="shared"` and `sdk_publishable = "partner"`.
	"idk_cc_source_library":    "sdk_source_set",
	"idk_cc_source_library_zx": "zx_library", // With `sdk="source"` and `sdk_publishable = "partner"`.
	"idk_cc_static_library":    "sdk_static_library",
	"idk_cc_static_library_zx": "zx_library", // With `sdk="static"` and `sdk_publishable = "partner"`.
	"idk_host_tool":            "sdk_host_tool",

	// Other
	"fidlgentest_go_test": "fidlgentest_go_test",
	"install_host_tools":  "install_host_tools",
	"genrule":             "action",
	"package":             "package",
	"test_suite":          "group",

	// `exports_files()` is a concept specific to Bazel, so there is no need to convert it.
	"exports_files": "__NO_GN_EQUIVALENT__",
}

// attrsToOmitByRules stores a mapping from known Bazel rules to attributes to
// omit when converting them to GN.
var attrsToOmitByRules = map[string]map[string]bool{
	"go_library": {
		// In GN we default cgo to true when compiling Go code, and explicitly disable
		// it in very few places. However, in Bazel, cgo defaults to false, and
		// require users to explicitly set when C sources are included.
		"cgo": true,
	},
	"genrule": {
		// bazel2gn ignores the `tools` attribute of genrule, and tries to parse it out of the `cmd`
		// attribute. It is the caller's responsibility to ensure that the `cmd` attribute contains
		// the correct tools. Also note since `genrule` is converted to `action` in GN, the `cmd`
		// attribute is converted to `script` and `args` in GN, so only one `tool` is supported.
		"tools": true,
	},
	"cc_library": {
		// TODO(https://fxbug.dev/457605523): Support `includes` conversion to `configs` in GN.
		// Currently the only use case is to set `includes = ["../.."]`, which is covered by
		// `"//build/config:default_include_dirs"` in GN.
		"includes": true,
	},
}

// Common Bazel attributes that use different names in GN.
var commonAttrMap = map[string]string{
	"srcs": "sources",
	"hdrs": "public",
}

// ccCommonAttrMap maps from attribute names common in Bazel CC rules to GN parameter names.
// This map only includes attributes that have different names in Bazel and GN.
var ccCommonAttrMap = map[string]string{
	"copts": "configs", // Strings are converted to configs by `bazelCOptToGNConfig()`.
}

// ccLibAttrMap maps from attribute names in Bazel CC library rules to GN parameter names.
// This map only includes attributes that have different names in Bazel and GN.
var ccLibAttrMap = mustMergeMaps(ccCommonAttrMap, map[string]string{
	"deps":                "public_deps",
	"implementation_deps": "deps",
})

// rustAttrMap maps from attribute name in Bazel Rust rules to GN parameter names.
// This map only includes attributes that have different names in Bazel and GN.
var rustAttrMap = map[string]string{
	"crate_features": "features",
}

// fidlAttrMap maps from attribute names in Bazel FIDL rules to GN parameter names.
// This map only includes attributes that have different names in Bazel and GN.
var fidlAttrMap = map[string]string{
	"deps":         "public_deps",
	"library_name": "name",
}

// idkAttrMap maps from attribute name in Bazel IDK rules to GN parameter names.
// This map only includes attributes that have different names in Bazel and GN.
var idkAttrMap = map[string]string{
	"api_area": "sdk_area",
	"idk_name": "sdk_name",

	// This renames the variable, but it must be made a `+=` inside a
	// conditional block in `attrAssignmentToGN()`. `deps` must be specified
	// before it in the `BUILD.bazel` file. This requires using
	// `# buildifier: leave-alone` above the macro.
	"fuchsia_deps": "public_deps",

	// These are not identical because files in `hdrs_for_internal_use` need
	// to be added to GN's `public` as well. This takes care of populating
	// `sdk_headers_for_internal_use`, and `attrAssignmentToGN()` adds the files
	// to `public`.
	"hdrs_for_internal_use": "sdk_headers_for_internal_use",
}

// Maps from attribute name in Bazel IDK rules to GN `zx_library()` parameter
// names for attributes unique to `zx_library()` instances that are in the IDK.
// Use `idkZxAttrMap` instead.
// This map only includes attributes that have different names in Bazel and GN.
var zxInIDKAttrMap = map[string]string{
	"category": "sdk_publishable",
}

// hostToolAttrMap maps from attribute name in Bazel host tool rules to GN parameter names.
var hostToolAttrMap = map[string]string{
	"implementation_deps": "deps",
	"tool_output_names":   "outputs",
}

// genruleAttrMap maps from attribute name in Bazel genrule to GN parameter names.
var genruleAttrMap = map[string]string{
	"outs": "outputs",
}

// idkAttrMap maps from attribute name in Bazel IDK C++ rules to GN parameter names.
var idkCcAttrMap = mustMergeMaps(idkAttrMap, ccLibAttrMap)

// idkAttrMap maps from attribute name in Bazel IDK C++ ZX rules to GN parameter names.
var idkZxAttrMap = mustMergeMaps(idkCcAttrMap, zxInIDKAttrMap)

// idkHostToolAttrMap maps from attribute name in Bazel IDK host tool rules to GN parameter names.
var idkHostToolAttrMap = mustMergeMaps(idkAttrMap, hostToolAttrMap)

// A mapping from Bazel rule names to attribute mappings.
// Attribute mappings map from Bazel rule attributes that use different names in GN.
var attrMapsByRules = map[string]map[string]string{
	// C++
	"cc_library": ccLibAttrMap,
	"cc_binary":  ccCommonAttrMap,

	// C++ Zircon
	"cc_shared_library_zx": ccLibAttrMap,
	"cc_source_library_zx": ccLibAttrMap,
	"cc_static_library_zx": ccLibAttrMap,

	// Rust
	"rust_binary":     rustAttrMap,
	"rust_library":    rustAttrMap,
	"rust_proc_macro": rustAttrMap,
	"rustc_binary":    rustAttrMap,
	"rustc_library":   rustAttrMap,

	// FIDL
	"fidl_library":        fidlAttrMap,
	"zither_fidl_library": fidlAttrMap,

	// IDK
	"idk_cc_shared_library":    idkCcAttrMap,
	"idk_cc_shared_library_zx": idkZxAttrMap,
	"idk_cc_source_library":    idkCcAttrMap,
	"idk_cc_source_library_zx": idkZxAttrMap,
	"idk_cc_static_library":    idkCcAttrMap,
	"idk_cc_static_library_zx": idkZxAttrMap,
	"idk_host_tool":            idkHostToolAttrMap,

	// Tools
	"install_host_tools": hostToolAttrMap,

	// Others
	"genrule": genruleAttrMap,
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

var bazelConstraintsToGNConditions = map[string]string{
	"HOST_CONSTRAINTS": "is_host",
}

var thirdPartyRustCrateRE = regexp.MustCompile(`^"\/\/third_party\/rust_crates.+:`)

// thirdPartyBazelRepos maps from Bazel third-party repository names to their GN equivalent
// dependency paths. The key is the Bazel repository name, and the value is the GN dependency
// path.
var thirdPartyBazelRepos = map[string]string{
	"@re2":                   "//third_party/re2",
	"@boringssl//src:crypto": "//third_party/boringssl:crypto",
}

// coptToConfig maps from Bazel copt values to configs to use in GN.
var coptToConfig = map[string]string{
	"-Wno-implicit-fallthrough": "//build/config:Wno-implicit-fallthrough",
}

// attrGNAssignmentOps maps from GN attribute names to the assignment operators to use in GN.
//
// NOTE: Entries in this map should be clearly documented.
var attrGNAssignmentOps = map[string]string{
	// `configs` in GN are rarely (never?) empty lists because we set them in BUILDCONFIG.gn.
	// Trying to overwrite a non-empty list in GN with a non-empty value will fail.
	// Simply replacing assignment with `+=` works for the initial use cases we need.
	// More complex mechanism may be required if we need to selectively overwrite config assignments.
	"configs": "+=",
}

// goThirdPartyAggregateDeps lists known third-party GN targets that include sources of multiple Go
// libraries in a single target. These deps need to be handled specially because in Bazel we have a
// 1:1 mapping between go_library targets and Go libraries.
var goThirdPartyAggregateDeps = []string{
	"//third_party/golibs:golang.org/x/crypto",
	"//third_party/golibs:gonum.org/v1/gonum",
	"//third_party/golibs:google.golang.org/protobuf",
	"//third_party/golibs:gvisor.dev/gvisor",
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
