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

// internalFxTestPackageRule is an internal rule identifier for an
// `fx_package()` with `test_components`, which is emitted as a GN
// `fuchsia_test_package()`. `fx_package()` targets without `test_components`
// have no GN equivalent. Because `$` is invalid in Starlark identifiers, user
// `BUILD.bazel` files can never invoke it directly.
const internalFxTestPackageRule = "$fx_test_package"

// internalFxTestComponentRule is an internal rule identifier for an
// `fx_test_component()` listed in the `test_components` of a converted
// `fx_package()` in the same file, which is emitted as a GN
// `fuchsia_test_component()`. Other `fx_test_component()` targets have no GN
// equivalent.
const internalFxTestComponentRule = "$fx_test_component"

// internalFxUnittestComponentRule is an internal rule identifier for an
// `fx_unittest_component()` listed in the `test_components` of a converted
// `fx_package()` in the same file, which is emitted as a GN
// `fuchsia_unittest_component()`. Other `fx_unittest_component()` targets have
// no GN equivalent.
const internalFxUnittestComponentRule = "$fx_unittest_component"

// fxTestArg is the name of the internal argument that carries the
// `fx_test()` call for a converted `fx_package()`, so its test specs can be
// set on the GN `fuchsia_test_package()`.
const fxTestArg = "$fx_test"

// gnEnvironmentLabelPrefix is the Bazel package of the `fx_test_environment()`
// targets that mirror the environments in //build/testing/environments.gni
// under the same names.
const gnEnvironmentLabelPrefix = "//build/testing/environments:"

// resolveTestPackages prepares a parsed `BUILD.bazel` file so that test
// packages convert to GN:
//
//  1. Every un-skipped `fx_package()` with un-skipped `test_components` is
//     marked to be converted to a GN `fuchsia_test_package()`, and gets the
//     un-skipped `fx_test()` in the same file whose `package` refers to it.
//  2. Every un-skipped `fx_test_component()` or `fx_unittest_component()`
//     listed in the `test_components` of such an `fx_package()` is marked to be
//     converted to a GN `fuchsia_test_component()` or
//     `fuchsia_unittest_component()`. Any other test component in the same
//     file is an error.
//
// This must run before `resolveComponentTargets()`, which handles the
// manifests and `fx_packaged_binary()` deps of the marked components.
func resolveTestPackages(f *syntax.File) error {
	fxTests := map[string][]*syntax.CallExpr{}
	for _, stmt := range f.Stmts {
		call, ok := callOfStmt(stmt)
		if !ok {
			continue
		}
		if fn, ok := call.Fn.(*syntax.Ident); !ok || fn.Name != "fx_test" {
			continue
		}
		skip, err := hasSkipAnnotation(stmt)
		if err != nil {
			return err
		}
		if skip {
			continue
		}
		pkg, attrSkip, ok := callLocalLabelAttr(call, "package")
		if !ok || attrSkip {
			continue
		}
		name := strings.TrimPrefix(pkg, ":")
		fxTests[name] = append(fxTests[name], call)
	}

	// Maps the names of test components of converted packages to the
	// `test_type` of the package's `fx_test()`, if any.
	testComponents := map[string]string{}
	testComponentPackages := map[string]syntax.Stmt{}
	for _, stmt := range f.Stmts {
		call, ok := callOfStmt(stmt)
		if !ok {
			continue
		}
		if fn, ok := call.Fn.(*syntax.Ident); !ok || fn.Name != "fx_package" {
			continue
		}
		skip, err := hasSkipAnnotation(stmt)
		if err != nil {
			return err
		}
		if skip {
			continue
		}
		components, err := unskippedTestComponents(call)
		if err != nil {
			return stmtError(stmt, err)
		}
		if len(components) == 0 {
			continue
		}
		name, _ := callStringAttr(call, "name")
		tests := fxTests[name]
		if len(tests) > 1 {
			return stmtError(stmt, fmt.Errorf("fx_package %q is the package of %d fx_test targets, but its GN copy can only have the test specs of one; use # @bazel2gn:skip and write the GN copy by hand", name, len(tests)))
		}
		// GN only supports `test_type` on `fuchsia_test_component()`, so
		// `fx_test(test_type = ...)` is set on each test component instead.
		var testType string
		call.Fn.(*syntax.Ident).Name = internalFxTestPackageRule
		if len(tests) == 1 {
			call.Args = append(call.Args, &syntax.BinaryExpr{
				X:  &syntax.Ident{Name: fxTestArg},
				Op: syntax.EQ,
				Y:  tests[0],
			})
			if tt, ok := unskippedCallStringAttr(tests[0], "test_type"); ok {
				testType = tt
			}
		}
		for _, c := range components {
			if !strings.HasPrefix(c, ":") {
				if testType != "" {
					return stmtError(stmt, fmt.Errorf("fx_package %q has test component %q from another package, so the test_type of its fx_test can't be set on it in GN; use # @bazel2gn:skip and write the GN copy by hand", name, c))
				}
				continue
			}
			cName := strings.TrimPrefix(c, ":")
			if prev, ok := testComponents[cName]; ok && prev != testType {
				return stmtError(stmt, fmt.Errorf("test component %q is in fx_packages whose fx_tests have different test_type (%q and %q), but its GN copy can only have one; use # @bazel2gn:skip and write the GN copy by hand", c, prev, testType))
			}
			testComponents[cName] = testType
			testComponentPackages[cName] = stmt
		}
	}

	resolved := map[string]bool{}
	for _, stmt := range f.Stmts {
		call, ok := callOfStmt(stmt)
		if !ok {
			continue
		}
		fn, ok := call.Fn.(*syntax.Ident)
		if !ok {
			continue
		}
		var internalRule string
		switch fn.Name {
		case "fx_test_component":
			internalRule = internalFxTestComponentRule
		case "fx_unittest_component":
			internalRule = internalFxUnittestComponentRule
		default:
			continue
		}
		skip, err := hasSkipAnnotation(stmt)
		if err != nil {
			return err
		}
		if skip {
			continue
		}
		name, _ := callStringAttr(call, "name")
		testType, ok := testComponents[name]
		if !ok {
			continue
		}
		resolved[name] = true
		ruleName := fn.Name
		fn.Name = internalRule
		if testType == "" {
			continue
		}
		if own, ok := unskippedCallStringAttr(call, "test_type"); ok {
			if own != testType {
				return stmtError(stmt, fmt.Errorf("%s %q test_type %q conflicts with test_type %q of its fx_test", ruleName, name, own, testType))
			}
			continue
		}
		call.Args = append(call.Args, &syntax.BinaryExpr{
			X:  &syntax.Ident{Name: "test_type"},
			Op: syntax.EQ,
			Y:  &syntax.Literal{Token: syntax.STRING, Raw: strconv.Quote(testType), Value: testType},
		})
	}

	// Every same-file test component of a converted package must be converted
	// too, or the GN package would refer to a target that doesn't exist.
	for name := range testComponents {
		if !resolved[name] {
			pkgStmt := testComponentPackages[name]
			return stmtError(pkgStmt, fmt.Errorf("test component \":%s\" isn't an un-skipped fx_test_component or fx_unittest_component in the same BUILD.bazel, so bazel2gn can't convert it to GN; use # @bazel2gn:skip on the fx_package and write the GN copy by hand", name))
		}
	}
	return nil
}

// unskippedTestComponents returns the labels in the un-skipped
// `test_components` of an `fx_package()` call, skipping elements annotated
// with `# @bazel2gn:skip`.
func unskippedTestComponents(call *syntax.CallExpr) ([]string, error) {
	for _, arg := range call.Args {
		name, ok := attrName(arg)
		if !ok || name != "test_components" {
			continue
		}
		skip, err := hasSkipAnnotation(arg)
		if err != nil {
			return nil, err
		}
		if skip {
			return nil, nil
		}
		listExpr, ok := unwrapParenExpr(arg.(*syntax.BinaryExpr).Y).(*syntax.ListExpr)
		if !ok {
			return nil, fmt.Errorf("expected list literal for fx_package test_components, got %T; use # @bazel2gn:skip and write the GN copy by hand", arg.(*syntax.BinaryExpr).Y)
		}
		var labels []string
		for _, elm := range listExpr.List {
			skip, err := hasSkipAnnotation(elm)
			if err != nil {
				return nil, err
			}
			if skip {
				continue
			}
			label, err := localTargetLabelValue(elm)
			if err != nil {
				return nil, fmt.Errorf("fx_package test_components: %v", err)
			}
			labels = append(labels, label)
		}
		return labels, nil
	}
	return nil, nil
}

// fxTestPackageToGN converts an `fx_package()` with `test_components` (and
// the `fx_test()` for it in the same file, if any) to a GN
// `fuchsia_test_package()`.
//
// Like for `fx_unittest_package()`, the GN package name has a `-gn` suffix so
// both copies can be published to the same package repository.
//
// Only the attributes that have a direct GN equivalent are supported. Other
// attributes are errors, so tests using them need `# @bazel2gn:skip` and a
// hand-written GN copy.
func fxTestPackageToGN(call *syntax.CallExpr) ([]string, error) {
	var name, packageName string
	var testComponents, components, subpackages, visArg *syntax.BinaryExpr
	var fxTest *syntax.CallExpr

	for _, arg := range call.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			return nil, fmt.Errorf("fx_package only accepts keyword arguments, got %T", arg)
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
				return nil, fmt.Errorf("fx_package name: %v", err)
			}
		case "package_name":
			packageName, err = stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return nil, fmt.Errorf("fx_package package_name: %v", err)
			}
		case "test_components":
			testComponents = binaryExpr
		case "components":
			components = binaryExpr
		case "subpackages":
			subpackages = binaryExpr
		case "visibility":
			visArg = binaryExpr
		case fxTestArg:
			fxTest = binaryExpr.Y.(*syntax.CallExpr)
		case "tags", "testonly":
			// GN test packages are always testonly, and Bazel tags have no GN
			// meaning for test packages.
		default:
			return nil, fmt.Errorf("bazel2gn doesn't support attribute %q of fx_package with test_components; use # @bazel2gn:skip and write the GN copy by hand", ident.Name)
		}
	}

	if name == "" {
		return nil, errors.New("missing `name` attribute in fx_package")
	}
	if testComponents == nil {
		return nil, fmt.Errorf("missing `test_components` attribute in fx_package %q", name)
	}
	if packageName == "" {
		packageName = name
	}

	body := []string{fmt.Sprintf("package_name = %s", strconv.Quote(packageName+gnTestPackageNameSuffix))}
	for _, attr := range []struct {
		gnName string
		arg    *syntax.BinaryExpr
	}{
		{"test_components", testComponents},
		// GN `fuchsia_test_package()` takes non-test components in `deps`.
		{"deps", components},
		{"subpackages", subpackages},
	} {
		if attr.arg == nil {
			continue
		}
		lines, err := exprToGN(attr.arg.Y, []transformer{bazelDepToGN})
		if err != nil {
			return nil, fmt.Errorf("converting fx_package %q %s: %v", name, attr.gnName, err)
		}
		lines[0] = attr.gnName + " = " + lines[0]
		body = append(body, lines...)
	}

	if fxTest != nil {
		testSpecs, err := fxTestSpecsToGN(fxTest)
		if err != nil {
			return nil, err
		}
		body = append(body, testSpecs...)
	}

	if visArg != nil {
		lines, err := attrAssignmentToGN(visArg, "fx_package", nil /*compiledActionOutputs*/, false /*appendAspectHints*/, false /*hasWithUnitTests*/)
		if err != nil {
			return nil, fmt.Errorf("converting fx_package visibility: %v", err)
		}
		body = append(body, lines...)
	}

	ret := []string{fmt.Sprintf("fuchsia_test_package(%s) {", strconv.Quote(name))}
	ret = append(ret, indent(body, 1)...)
	ret = append(ret, "}")
	return ret, nil
}

// fxTestSpecsToGN converts the attributes of an `fx_test()` call to the
// `test_specs` of a GN `fuchsia_test_package()`.
func fxTestSpecsToGN(call *syntax.CallExpr) ([]string, error) {
	testName, _ := callStringAttr(call, "name")
	var specs []string
	for _, arg := range call.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			return nil, fmt.Errorf("fx_test %q only accepts keyword arguments, got %T", testName, arg)
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
		case "max_log_severity":
			severity, err := stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return nil, fmt.Errorf("fx_test %q max_log_severity: %v", testName, err)
			}
			specs = append(specs,
				"log_settings = {",
				indentPrefix+fmt.Sprintf("max_severity = %s", strconv.Quote(severity)),
				"}",
			)
		case "build_only":
			rhs, ok := unwrapParenExpr(binaryExpr.Y).(*syntax.Ident)
			if !ok || (rhs.Name != "True" && rhs.Name != "False") {
				return nil, fmt.Errorf("fx_test %q build_only must be True or False", testName)
			}
			if rhs.Name == "True" {
				specs = append(specs, "build_only = true")
			}
		case "environments":
			envs, err := fxTestEnvironmentsToGN(binaryExpr.Y)
			if err != nil {
				return nil, fmt.Errorf("fx_test %q environments: %v; use # @bazel2gn:skip on the fx_package and write the GN copy by hand", testName, err)
			}
			specs = append(specs, "environments = [")
			for _, env := range envs {
				specs = append(specs, indentPrefix+env+",")
			}
			specs = append(specs, "]")
		case "test_type":
			// Set on each test component by `resolveTestPackages()`.
			if _, err := stringLiteralValue(binaryExpr.Y); err != nil {
				return nil, fmt.Errorf("fx_test %q test_type: %v; use # @bazel2gn:skip on the fx_package and write the GN copy by hand", testName, err)
			}
		case "name", "package", "tags", "testonly", "visibility":
			// These don't affect the GN test specs.
		default:
			return nil, fmt.Errorf("bazel2gn doesn't support attribute %q of fx_test %q; use # @bazel2gn:skip on the fx_package and write the GN copy by hand", ident.Name, testName)
		}
	}
	if len(specs) == 0 {
		return nil, nil
	}
	ret := []string{"test_specs = {"}
	ret = append(ret, indent(specs, 1)...)
	ret = append(ret, "}")
	return ret, nil
}

// fxTestEnvironmentsToGN converts a list literal of
// `//build/testing/environments:<env>` labels to the names of the same
// environments in //build/testing/environments.gni.
func fxTestEnvironmentsToGN(expr syntax.Expr) ([]string, error) {
	listExpr, ok := unwrapParenExpr(expr).(*syntax.ListExpr)
	if !ok {
		return nil, fmt.Errorf("expected list literal, got %T", expr)
	}
	var envs []string
	for _, elm := range listExpr.List {
		label, err := stringLiteralValue(elm)
		if err != nil {
			return nil, err
		}
		env, ok := strings.CutPrefix(label, gnEnvironmentLabelPrefix)
		if !ok || env == "" {
			return nil, fmt.Errorf("only environments in %q have GN equivalents, got %q", strings.TrimSuffix(gnEnvironmentLabelPrefix, ":"), label)
		}
		envs = append(envs, env)
	}
	return envs, nil
}

// fxUnittestComponentToGN converts an `fx_unittest_component()` listed in the
// `test_components` of a converted `fx_package()` to a GN
// `fuchsia_unittest_component()`, which generates the same manifest.
func fxUnittestComponentToGN(call *syntax.CallExpr) ([]string, error) {
	var name string
	var body []string
	for _, arg := range call.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			return nil, fmt.Errorf("fx_unittest_component only accepts keyword arguments, got %T", arg)
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
				return nil, fmt.Errorf("fx_unittest_component name: %v", err)
			}
		case "component_name", "test_type":
			val, err := stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return nil, fmt.Errorf("fx_unittest_component %s: %v", ident.Name, err)
			}
			body = append(body, fmt.Sprintf("%s = %s", ident.Name, strconv.Quote(val)))
		case "binary":
			lines, err := exprToGN(&syntax.ListExpr{List: []syntax.Expr{binaryExpr.Y}}, []transformer{bazelDepToGN})
			if err != nil {
				return nil, fmt.Errorf("converting fx_unittest_component binary: %v", err)
			}
			lines[0] = "deps = " + lines[0]
			body = append(body, lines...)
		case "visibility":
			lines, err := attrAssignmentToGN(binaryExpr, "fx_unittest_component", nil /*compiledActionOutputs*/, false /*appendAspectHints*/, false /*hasWithUnitTests*/)
			if err != nil {
				return nil, fmt.Errorf("converting fx_unittest_component visibility: %v", err)
			}
			body = append(body, lines...)
		case "tags", "testonly":
			// GN test components are always testonly, and Bazel tags have no
			// GN meaning for test components.
		default:
			return nil, fmt.Errorf("bazel2gn doesn't support attribute %q of fx_unittest_component; use # @bazel2gn:skip on the fx_package and write the GN copy by hand", ident.Name)
		}
	}
	if name == "" {
		return nil, errors.New("missing `name` attribute in fx_unittest_component")
	}
	ret := []string{fmt.Sprintf("fuchsia_unittest_component(%s) {", strconv.Quote(name))}
	ret = append(ret, indent(body, 1)...)
	ret = append(ret, "}")
	return ret, nil
}
