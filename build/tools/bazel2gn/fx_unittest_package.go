// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn

import (
	"errors"
	"fmt"
	"strconv"
	"strings"

	"go.starlark.net/syntax"
)

// gnTestPackageNameSuffix is appended to the package name of the GN copy of an
// `fx_unittest_package()`. Both copies are published to the same package
// repository, so they can't share a package name. The Bazel copy keeps the
// original name, so migrating a test to Bazel doesn't change its URL.
const gnTestPackageNameSuffix = "-gn"

// fxUnittestPackageToGN converts a Bazel `fx_unittest_package()` macro call to
// GN targets: a `fuchsia_unittest_package()` for a single test binary, or
// `fuchsia_unittest_component()`s in a `fuchsia_test_package()` for several.
//
// Only the attributes that have a direct GN equivalent are supported. Other
// attributes are errors, so tests using them need `# @bazel2gn:skip` and a
// hand-written GN copy.
func fxUnittestPackageToGN(call *syntax.CallExpr) ([]string, error) {
	var name, packageName, maxLogSeverity string
	var unitTests *syntax.ListExpr
	var visArg *syntax.BinaryExpr

	for _, arg := range call.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			return nil, fmt.Errorf("fx_unittest_package only accepts keyword arguments, got %T", arg)
		}
		shouldSkip, err := hasSkipAnnotation(binaryExpr)
		if err != nil {
			return nil, fmt.Errorf("failed to check skip annotation: %v", err)
		}
		if shouldSkip {
			continue
		}
		ident, ok := binaryExpr.X.(*syntax.Ident)
		if !ok {
			return nil, fmt.Errorf("expected identifier for argument name, got %T", binaryExpr.X)
		}

		switch ident.Name {
		case "name":
			name, err = stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return nil, fmt.Errorf("fx_unittest_package name: %v", err)
			}
		case "package_name":
			packageName, err = stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return nil, fmt.Errorf("fx_unittest_package package_name: %v", err)
			}
		case "max_log_severity":
			maxLogSeverity, err = stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return nil, fmt.Errorf("fx_unittest_package max_log_severity: %v", err)
			}
		case "unit_tests":
			listExpr, ok := unwrapParenExpr(binaryExpr.Y).(*syntax.ListExpr)
			if !ok {
				return nil, fmt.Errorf("expected list literal for fx_unittest_package unit_tests, got %T", binaryExpr.Y)
			}
			var unskipped []syntax.Expr
			for _, elm := range listExpr.List {
				shouldSkip, err := hasSkipAnnotation(elm)
				if err != nil {
					return nil, fmt.Errorf("failed to check skip annotation: %v", err)
				}
				if !shouldSkip {
					unskipped = append(unskipped, elm)
				}
			}
			if len(unskipped) == 0 {
				return nil, errors.New("fx_unittest_package unit_tests must not be empty")
			}
			unitTests = &syntax.ListExpr{List: unskipped}
		case "visibility":
			visArg = binaryExpr
		case "tags", "testonly":
			// GN test packages are always testonly, and Bazel tags have no GN
			// meaning for test packages.
		default:
			return nil, fmt.Errorf("bazel2gn doesn't support attribute %q of fx_unittest_package; use # @bazel2gn:skip and write the GN copy by hand", ident.Name)
		}
	}

	if name == "" {
		return nil, errors.New("missing `name` attribute in fx_unittest_package")
	}
	if unitTests == nil {
		return nil, errors.New("missing `unit_tests` attribute in fx_unittest_package")
	}
	if packageName == "" {
		packageName = name
	}

	var ret []string
	var packageBody []string
	packageBody = append(packageBody, fmt.Sprintf("package_name = %s", strconv.Quote(packageName+gnTestPackageNameSuffix)))

	if len(unitTests.List) == 1 {
		// Like the Bazel macro, a single test binary gets a component named
		// after the package. Unlike Bazel, the GN package name has a suffix, so
		// set the component name explicitly.
		depLines, err := exprToGN(unitTests, []transformer{bazelDepToGN})
		if err != nil {
			return nil, fmt.Errorf("converting fx_unittest_package unit_tests: %v", err)
		}
		depLines[0] = "deps = " + depLines[0]
		packageBody = append(packageBody, fmt.Sprintf("component_name = %s", strconv.Quote(packageName)))
		packageBody = append(packageBody, depLines...)
		ret = append(ret, fmt.Sprintf("fuchsia_unittest_package(%s) {", strconv.Quote(name)))
	} else {
		// GN's `fuchsia_unittest_package()` only supports a single test
		// binary, so use one `fuchsia_unittest_component()` per binary in a
		// `fuchsia_test_package()`, named like the Bazel macro names them.
		var components []string
		for _, elm := range unitTests.List {
			label, err := stringLiteralValue(elm)
			if err != nil {
				return nil, fmt.Errorf("fx_unittest_package unit_tests: %v", err)
			}
			testName := label[strings.LastIndexAny(label, ":/")+1:]
			componentTarget := fmt.Sprintf("%s.%s.component", name, testName)
			depLines, err := exprToGN(&syntax.ListExpr{List: []syntax.Expr{elm}}, []transformer{bazelDepToGN})
			if err != nil {
				return nil, fmt.Errorf("converting fx_unittest_package unit_tests: %v", err)
			}
			depLines[0] = "deps = " + depLines[0]
			ret = append(ret, fmt.Sprintf("fuchsia_unittest_component(%s) {", strconv.Quote(componentTarget)))
			ret = append(ret, indent([]string{
				fmt.Sprintf("component_name = %s", strconv.Quote(testName)),
				"visibility = [ \":*\" ]",
			}, 1)...)
			ret = append(ret, indent(depLines, 1)...)
			ret = append(ret, "}")
			components = append(components, strconv.Quote(":"+componentTarget)+",")
		}
		packageBody = append(packageBody, "test_components = [")
		packageBody = append(packageBody, indent(components, 1)...)
		packageBody = append(packageBody, "]")
		ret = append(ret, fmt.Sprintf("fuchsia_test_package(%s) {", strconv.Quote(name)))
	}
	ret = append(ret, indent(packageBody, 1)...)

	if maxLogSeverity != "" {
		ret = append(ret, indent([]string{
			"test_specs = {",
			indentPrefix + "log_settings = {",
			indentPrefix + indentPrefix + fmt.Sprintf("max_severity = %s", strconv.Quote(maxLogSeverity)),
			indentPrefix + "}",
			"}",
		}, 1)...)
	}

	if visArg != nil {
		lines, err := attrAssignmentToGN(visArg, "fx_unittest_package", nil /*compiledActionOutputs*/, false /*appendAspectHints*/, false /*hasWithUnitTests*/)
		if err != nil {
			return nil, fmt.Errorf("converting fx_unittest_package visibility: %v", err)
		}
		ret = append(ret, indent(lines, 1)...)
	}

	ret = append(ret, "}")
	return ret, nil
}
