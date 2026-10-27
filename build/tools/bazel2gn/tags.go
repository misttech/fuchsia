// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn

import (
	"fmt"
	"strings"

	"go.starlark.net/syntax"
)

// assertNoDepsTagPrefix is the prefix of Bazel tags listing labels the target
// must not depend on, see //build/bazel/aspects/assert_no_deps.bzl.
const assertNoDepsTagPrefix = "assert_no_deps="

// excludeToolchainTagsTagPrefix is the prefix of Bazel tags listing toolchain
// tags (e.g. variants such as `heapdump`) that the GN copy of a Rust target
// must not be built with, see `exclude_toolchain_tags` in
// //build/config/BUILDCONFIG.gn. Bazel ignores these tags.
//
// TODO(https://fxbug.dev/578022348): Add canonical Bazel support for
// excluding Rust targets from toolchain variants (e.g. heapdump).
const excludeToolchainTagsTagPrefix = "exclude_toolchain_tags="

// excludeToolchainTagsGNTemplates are the GN templates that accept
// `exclude_toolchain_tags` from `excludeToolchainTagsTagPrefix` tags. C++
// binaries use `disable_instrumentation` instead.
var excludeToolchainTagsGNTemplates = map[string]bool{
	"rustc_binary":  true,
	"rustc_dylib":   true,
	"rustc_library": true,
	"rustc_test":    true,
}

// tagsToGN converts Bazel tags attribute to GN assignments if known syncable tags are found.
func tagsToGN(expr syntax.Expr, bazelRule string, hasWithUnitTests bool) ([]string, error) {
	// Currently only list literals are supported, i.e. select is not supported yet.
	listExpr, ok := expr.(*syntax.ListExpr)
	if !ok {
		return nil, fmt.Errorf("expected list literal for tags attribute, got %T", expr)
	}

	var assertNoDeps []syntax.Expr
	var excludeToolchainTags []syntax.Expr
	for _, elm := range listExpr.List {
		lit, ok := elm.(*syntax.Literal)
		if !ok {
			return nil, fmt.Errorf("expected list item to be a literal in tags attribute, got %T", elm)
		}
		// lit.Raw is quoted, so unquote.
		val := lit.Raw[1 : len(lit.Raw)-1]
		if toolchainTag, ok := strings.CutPrefix(val, excludeToolchainTagsTagPrefix); ok {
			gnTemplate := bazelRuleToGNTemplate[bazelRule]
			if !excludeToolchainTagsGNTemplates[gnTemplate] {
				return nil, fmt.Errorf("the %q tag is only supported on Rust binary, library, and test rules, got %s", val, bazelRule)
			}
			if gnTemplate == "rustc_library" && !hasWithUnitTests {
				return nil, fmt.Errorf("the %q tag is only supported on %s when with_unit_tests is set", val, bazelRule)
			}
			if toolchainTag == "" {
				return nil, fmt.Errorf("empty %q tag", excludeToolchainTagsTagPrefix)
			}
			excludeToolchainTags = append(excludeToolchainTags, &syntax.Literal{Raw: gnQuoteLiteral(toolchainTag)})
			continue
		}
		label, ok := strings.CutPrefix(val, assertNoDepsTagPrefix)
		if !ok {
			continue
		}
		// Requote the label for bazelDepToGN, preserving comments so
		// @bazel2gn:path_overwrite and @bazel2gn:raw_overwrite work on tags.
		depLit := &syntax.Literal{Raw: fmt.Sprintf(`"%s"`, label)}
		if c := lit.Comments(); c != nil {
			depLit.AllocComments()
			*depLit.Comments() = *c
		}
		transformedDep, err := bazelDepToGN(depLit)
		if err != nil {
			return nil, fmt.Errorf("transforming assert_no_deps label %q: %v", label, err)
		}
		transformedDep, err = bazelVisibilityToGN(transformedDep)
		if err != nil {
			return nil, fmt.Errorf("transforming assert_no_deps wildcard %q: %v", label, err)
		}
		assertNoDeps = append(assertNoDeps, transformedDep)
	}

	var ret []string
	for _, v := range []struct {
		name  string
		elems []syntax.Expr
	}{
		{"assert_no_deps", assertNoDeps},
		{"exclude_toolchain_tags", excludeToolchainTags},
	} {
		if len(v.elems) == 0 {
			continue
		}
		lines, err := listExprToGN(&syntax.ListExpr{List: v.elems}, nil)
		if err != nil {
			return nil, fmt.Errorf("converting %s list to GN: %v", v.name, err)
		}
		lines[0] = v.name + " = " + lines[0]
		ret = append(ret, lines...)
	}
	return ret, nil
}
