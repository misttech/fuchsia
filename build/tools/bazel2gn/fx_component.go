// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn

import (
	"errors"
	"fmt"
	"regexp"
	"strings"

	"go.starlark.net/syntax"
)

// internalRenamedBinaryRule is an internal rule identifier used when an
// `fx_packaged_binary()` target referenced by an `fx_component()` installs its
// binary at a non-default destination and must be emitted as a GN
// `renamed_binary()` target. Because `$` is invalid in Starlark identifiers,
// user `BUILD.bazel` files can never invoke it directly.
const internalRenamedBinaryRule = "$renamed_binary"

var (
	allowedFxComponentManifestAttrs = map[string]bool{
		"name":                   true,
		"manifest":               true,
		"component_name":         true,
		"includes":               true,
		"restricted_features":    true,
		"testonly":               true,
		"visibility":             true,
		"tags":                   true,
		"target_compatible_with": true,
	}

	// `fx_compiled_manifest()` accepts the same attributes as
	// `fx_component_manifest()`, plus `output_name`.
	allowedFxCompiledManifestAttrs = map[string]bool{
		"name":                   true,
		"manifest":               true,
		"component_name":         true,
		"includes":               true,
		"output_name":            true,
		"restricted_features":    true,
		"testonly":               true,
		"visibility":             true,
		"tags":                   true,
		"target_compatible_with": true,
	}

	allowedFxComponentAttrs = map[string]bool{
		"name":                   true,
		"compiled_manifest":      true,
		"component_name":         true,
		"check_includes":         true,
		"deps":                   true,
		"testonly":               true,
		"visibility":             true,
		"tags":                   true,
		"target_compatible_with": true,
	}

	// `fx_test_component()` accepts the same attributes as `fx_component()`,
	// plus `test_type`.
	allowedFxTestComponentAttrs = mustMergeAllowedAttrs(allowedFxComponentAttrs, map[string]bool{
		"test_type": true,
	})

	allowedFxPackagedBinaryAttrs = map[string]bool{
		"name":                   true,
		"binary":                 true,
		"binary_name":            true,
		"install_root":           true,
		"testonly":               true,
		"visibility":             true,
		"tags":                   true,
		"target_compatible_with": true,
	}

	rawOutputNameRE = regexp.MustCompile(`^\s*output_name\s*=\s*"([^"]+)"\s*$`)
)

// isFxManifestRule returns whether `rule` compiles a component manifest that
// GN targets can pass to `get_target_outputs()`.
func isFxManifestRule(rule string) bool {
	return rule == "fx_component_manifest" || rule == "fx_compiled_manifest"
}

// prepareFxCompiledManifest validates a Bazel `fx_compiled_manifest()` call and
// rewrites it in place before converting it to GN `cm()`.
//
// GN `cm()` has no `component_name`, so this replaces `component_name` and
// `output_name` with the `output_name` and `config_values_package_path` that
// the Bazel rule uses, so both build systems produce the same `.cm` file at
// the same path relative to the target's output directory.
func prepareFxCompiledManifest(call *syntax.CallExpr) error {
	var name, componentName, outputName string
	hasManifest := false
	newArgs := make([]syntax.Expr, 0, len(call.Args)+2)
	insertIdx := -1

	for _, arg := range call.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			return fmt.Errorf("fx_compiled_manifest only accepts keyword arguments, got %T", arg)
		}
		shouldSkip, err := hasSkipAnnotation(binaryExpr)
		if err != nil {
			return fmt.Errorf("failed to check skip annotation: %v", err)
		}
		if shouldSkip {
			newArgs = append(newArgs, arg)
			continue
		}
		ident, ok := binaryExpr.X.(*syntax.Ident)
		if !ok {
			return fmt.Errorf("expected identifier for argument name, got %T", binaryExpr.X)
		}
		if !allowedFxCompiledManifestAttrs[ident.Name] {
			return fmt.Errorf("bazel2gn doesn't support attribute %q of fx_compiled_manifest; use # @bazel2gn:skip and write the GN copy by hand", ident.Name)
		}
		switch ident.Name {
		case "name":
			name, err = stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return fmt.Errorf("fx_compiled_manifest name: %v", err)
			}
		case "manifest":
			hasManifest = true
			newArgs = append(newArgs, arg)
			insertIdx = len(newArgs)
			continue
		case "component_name":
			componentName, err = stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return fmt.Errorf("fx_compiled_manifest component_name: %v", err)
			}
			continue
		case "output_name":
			outputName, err = stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return fmt.Errorf("fx_compiled_manifest output_name: %v", err)
			}
			continue
		}
		newArgs = append(newArgs, arg)
	}

	if name == "" {
		return errors.New("missing `name` attribute in fx_compiled_manifest")
	}
	if !hasManifest {
		return fmt.Errorf("missing `manifest` attribute in fx_compiled_manifest %q", name)
	}
	if componentName == "" {
		componentName = name
	}
	if outputName == "" {
		// LINT.IfChange(fx_compiled_manifest_output_name)
		outputName = fmt.Sprintf("cml/%s/%s.cm", name, componentName)
		// LINT.ThenChange(//build/bazel/rules/components/fx_component.bzl:fx_compiled_manifest_output_name)
	}
	generated := []syntax.Expr{
		attrExpr("output_name", stringLiteral(outputName)),
		// `compile_component_manifest()` always passes `--config-package-path`.
		attrExpr("config_values_package_path", stringLiteral("meta/"+componentName+".cvf")),
	}
	call.Args = append(newArgs[:insertIdx:insertIdx], append(generated, newArgs[insertIdx:]...)...)
	return nil
}

// validateFxComponentManifest validates a Bazel `fx_component_manifest()` call
// before converting it to GN `fuchsia_component_manifest()`.
func validateFxComponentManifest(call *syntax.CallExpr) error {
	var name string
	hasManifest := false

	for _, arg := range call.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			return fmt.Errorf("fx_component_manifest only accepts keyword arguments, got %T", arg)
		}
		shouldSkip, err := hasSkipAnnotation(binaryExpr)
		if err != nil {
			return fmt.Errorf("failed to check skip annotation: %v", err)
		}
		if shouldSkip {
			continue
		}
		ident, ok := binaryExpr.X.(*syntax.Ident)
		if !ok {
			return fmt.Errorf("expected identifier for argument name, got %T", binaryExpr.X)
		}
		if !allowedFxComponentManifestAttrs[ident.Name] {
			return fmt.Errorf("bazel2gn doesn't support attribute %q of fx_component_manifest; use # @bazel2gn:skip and write the GN copy by hand", ident.Name)
		}
		switch ident.Name {
		case "name":
			name, err = stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return fmt.Errorf("fx_component_manifest name: %v", err)
			}
		case "manifest":
			hasManifest = true
		}
	}

	if name == "" {
		return errors.New("missing `name` attribute in fx_component_manifest")
	}
	if !hasManifest {
		return fmt.Errorf("missing `manifest` attribute in fx_component_manifest %q", name)
	}
	return nil
}

// validateFxComponent validates a Bazel `fx_component()` call before converting
// it to GN `fuchsia_component()`.
func validateFxComponent(call *syntax.CallExpr) error {
	rule := "fx_component"
	allowedAttrs := allowedFxComponentAttrs
	if fn, ok := call.Fn.(*syntax.Ident); ok && fn.Name == internalFxTestComponentRule {
		rule = "fx_test_component"
		allowedAttrs = allowedFxTestComponentAttrs
	}
	var name string
	var compiledManifestExpr syntax.Expr

	for _, arg := range call.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			return fmt.Errorf("%s only accepts keyword arguments, got %T", rule, arg)
		}
		shouldSkip, err := hasSkipAnnotation(binaryExpr)
		if err != nil {
			return fmt.Errorf("failed to check skip annotation: %v", err)
		}
		if shouldSkip {
			continue
		}
		ident, ok := binaryExpr.X.(*syntax.Ident)
		if !ok {
			return fmt.Errorf("expected identifier for argument name, got %T", binaryExpr.X)
		}
		if !allowedAttrs[ident.Name] {
			return fmt.Errorf("bazel2gn doesn't support attribute %q of %s; use # @bazel2gn:skip and write the GN copy by hand", ident.Name, rule)
		}
		switch ident.Name {
		case "name":
			name, err = stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return fmt.Errorf("%s name: %v", rule, err)
			}
		case "compiled_manifest":
			compiledManifestExpr = binaryExpr.Y
		}
	}

	if name == "" {
		return fmt.Errorf("missing `name` attribute in %s", rule)
	}
	if compiledManifestExpr == nil {
		return fmt.Errorf("missing `compiled_manifest` attribute in %s %q", rule, name)
	}

	cmTarget, err := localTargetLabelValue(compiledManifestExpr)
	if err != nil {
		return fmt.Errorf("%s %q compiled_manifest: %v", rule, name, err)
	}
	if !strings.HasPrefix(cmTarget, ":") {
		return fmt.Errorf("%s %q compiled_manifest %q must refer to a target in the same BUILD file (\":<target>\") because GN fuchsia_component calls get_target_outputs(cm_label); use # @bazel2gn:skip and write the GN copy by hand", rule, name, cmTarget)
	}
	return nil
}

// isConvertedFxComponentRule returns whether `rule` is converted to a GN
// `fuchsia_component()` or `fuchsia_test_component()`.
func isConvertedFxComponentRule(rule string) bool {
	return rule == "fx_component" || rule == internalFxTestComponentRule
}

// mustMergeAllowedAttrs returns the union of the allowed attribute sets.
func mustMergeAllowedAttrs(sets ...map[string]bool) map[string]bool {
	ret := map[string]bool{}
	for _, set := range sets {
		for k, v := range set {
			if _, ok := ret[k]; ok {
				panic(fmt.Sprintf("duplicate allowed attribute %q", k))
			}
			ret[k] = v
		}
	}
	return ret
}

// localTargetLabelValue returns the effective GN label string of a literal
// expression (accounting for `# @bazel2gn:path_overwrite:` if present).
func localTargetLabelValue(expr syntax.Expr) (string, error) {
	unwrapped := unwrapParenExpr(expr)
	if lit, ok := unwrapped.(*syntax.Literal); ok {
		if overwritten, hasOverwrite := overwrittenPath(lit); hasOverwrite {
			return overwritten, nil
		}
	}
	return stringLiteralValue(unwrapped)
}

type packagedBinaryRecord struct {
	stmt        *syntax.ExprStmt
	call        *syntax.CallExpr
	skipped     bool
	emittedName bool
}

// resolveComponentTargets prepares a parsed `BUILD.bazel` file so
// `fx_component_manifest()`, `fx_component()` and converted
// `fx_test_component()` targets (see `resolveTestPackages()`) convert cleanly
// to GN:
//
//  1. Skips any `fx_component_manifest()` statement referenced via
//     `compiled_manifest` by an unconverted `fx_test_component()` or a skipped
//     `fx_component()` in the same file when not referenced by any un-skipped
//     converted component or `fuchsia_structured_config_rust_lib()`.
//  2. Hoists any `fx_component_manifest()` statement that appears after an
//     earlier converted component or `fuchsia_structured_config_rust_lib()`
//     referencing it via `:manifest`, since GN's `get_target_outputs(cm_label)`
//     requires the manifest target to be evaluated earlier in `BUILD.gn`.
//  3. Resolves `fx_packaged_binary()` targets referenced by converted
//     component `deps`: unwrapping directly to `:<binary>` when
//     `install_root + binary_name` matches the GN binary's default
//     `"bin/<output_name>"`, or rewriting the `fx_packaged_binary()` into a GN
//     `renamed_binary()` target otherwise.
func resolveComponentTargets(f *syntax.File) error {
	if err := checkCompiledManifestConsumers(f); err != nil {
		return err
	}
	if err := skipBazelOnlyComponentManifests(f); err != nil {
		return err
	}
	if err := hoistComponentManifests(f); err != nil {
		return err
	}

	gnBinOutputs, err := indexGNBinaryOutputNames(f)
	if err != nil {
		return err
	}

	pkgBins := map[string]*packagedBinaryRecord{}
	varStmts := map[string]*syntax.AssignStmt{}
	for _, stmt := range f.Stmts {
		if assign, ok := stmt.(*syntax.AssignStmt); ok {
			if skip, err := hasSkipAnnotation(assign); err != nil {
				return err
			} else if !skip {
				if ident, ok := assign.LHS.(*syntax.Ident); ok {
					varStmts[ident.Name] = assign
				}
			}
			continue
		}
		exprStmt, ok := stmt.(*syntax.ExprStmt)
		if !ok {
			continue
		}
		call, ok := exprStmt.X.(*syntax.CallExpr)
		if !ok {
			continue
		}
		fn, ok := call.Fn.(*syntax.Ident)
		if !ok || fn.Name != "fx_packaged_binary" {
			continue
		}
		skip, err := hasSkipAnnotation(stmt)
		if err != nil {
			return err
		}
		name, ok := callStringAttr(call, "name")
		if !ok || name == "" {
			continue
		}
		pkgBins[name] = &packagedBinaryRecord{
			stmt:    exprStmt,
			call:    call,
			skipped: skip,
		}
	}

	visitedVars := map[string]bool{}
	for _, stmt := range f.Stmts {
		call, ok := callOfStmt(stmt)
		if !ok {
			continue
		}
		fn, ok := call.Fn.(*syntax.Ident)
		if !ok || !isConvertedFxComponentRule(fn.Name) {
			continue
		}
		skip, err := hasSkipAnnotation(stmt)
		if err != nil {
			return err
		}
		if skip {
			continue
		}
		compName, _ := callStringAttr(call, "name")
		for _, arg := range call.Args {
			argSkip, err := hasSkipAnnotation(arg)
			if err != nil {
				return err
			}
			if argSkip {
				continue
			}
			attr, ok := attrName(arg)
			if !ok || attr != "deps" {
				continue
			}
			kv := arg.(*syntax.BinaryExpr)
			rewritten, err := resolveComponentDepsExpr(kv.Y, compName, pkgBins, gnBinOutputs, varStmts, visitedVars)
			if err != nil {
				return stmtError(stmt, err)
			}
			kv.Y = rewritten
		}
	}

	return nil
}

// checkCompiledManifestConsumers fails if an un-skipped `fx_component()` uses an
// `fx_compiled_manifest()` in the same file as its `compiled_manifest`, because
// GN `fuchsia_component()` relies on `fuchsia_component_manifest()` to add the
// compiled manifest to the package, which GN `cm()` doesn't do.
func checkCompiledManifestConsumers(f *syntax.File) error {
	compiledManifests := map[string]bool{}
	for _, stmt := range f.Stmts {
		call, ok := callOfStmt(stmt)
		if !ok {
			continue
		}
		if fn, ok := call.Fn.(*syntax.Ident); ok && fn.Name == "fx_compiled_manifest" {
			if name, ok := callStringAttr(call, "name"); ok {
				compiledManifests[name] = true
			}
		}
	}
	if len(compiledManifests) == 0 {
		return nil
	}
	for _, stmt := range f.Stmts {
		call, ok := callOfStmt(stmt)
		if !ok {
			continue
		}
		fn, ok := call.Fn.(*syntax.Ident)
		if !ok || !isConvertedFxComponentRule(fn.Name) {
			continue
		}
		skip, err := hasSkipAnnotation(stmt)
		if err != nil {
			return err
		}
		if skip {
			continue
		}
		cmVal, attrSkip, ok := callLocalLabelAttr(call, "compiled_manifest")
		if !ok || attrSkip || !compiledManifests[strings.TrimPrefix(cmVal, ":")] {
			continue
		}
		compName, _ := callStringAttr(call, "name")
		return stmtError(stmt, fmt.Errorf("fx_component %q compiled_manifest %q refers to an fx_compiled_manifest(), which isn't packaged in GN; use fx_component_manifest() instead", compName, cmVal))
	}
	return nil
}

// skipBazelOnlyComponentManifests omits any un-skipped `fx_component_manifest`
// statement referenced via `compiled_manifest` by an unconverted
// `fx_test_component` or a skipped `fx_component` in the same file and not by
// any un-skipped converted component or `fuchsia_structured_config_rust_lib`.
func skipBazelOnlyComponentManifests(f *syntax.File) error {
	convertedConsumers := map[string]bool{}
	bazelOnlyConsumers := map[string]bool{}

	for _, stmt := range f.Stmts {
		call, ok := callOfStmt(stmt)
		if !ok {
			continue
		}
		fn, ok := call.Fn.(*syntax.Ident)
		if !ok {
			continue
		}
		var cmAttr string
		switch fn.Name {
		case "fx_component", "fx_test_component", internalFxTestComponentRule:
			cmAttr = "compiled_manifest"
		case "fuchsia_structured_config_rust_lib":
			cmAttr = "cm_label"
		default:
			continue
		}
		stmtSkip, err := hasSkipAnnotation(stmt)
		if err != nil {
			return err
		}
		cmVal, attrSkip, ok := callLocalLabelAttr(call, cmAttr)
		if !ok {
			continue
		}
		mName := strings.TrimPrefix(cmVal, ":")
		if fn.Name == "fx_test_component" || bazelRuleToGNTemplate[fn.Name] == noGNEquivalent || stmtSkip || attrSkip {
			bazelOnlyConsumers[mName] = true
		} else {
			convertedConsumers[mName] = true
		}
	}

	if len(bazelOnlyConsumers) == 0 {
		return nil
	}

	newStmts := make([]syntax.Stmt, 0, len(f.Stmts))
	for _, stmt := range f.Stmts {
		if call, ok := callOfStmt(stmt); ok {
			if fn, ok := call.Fn.(*syntax.Ident); ok && fn.Name == "fx_component_manifest" {
				skip, err := hasSkipAnnotation(stmt)
				if err != nil {
					return err
				}
				if !skip && !hasRawAttributeAnnotation(stmt) {
					if name, ok := callStringAttr(call, "name"); ok && bazelOnlyConsumers[name] && !convertedConsumers[name] {
						continue
					}
				}
			}
		}
		newStmts = append(newStmts, stmt)
	}
	f.Stmts = newStmts
	return nil
}

func callLocalLabelAttr(call *syntax.CallExpr, attr string) (val string, attrSkipped bool, ok bool) {
	for _, arg := range call.Args {
		if name, hasName := attrName(arg); hasName && name == attr {
			skip, err := hasSkipAnnotation(arg)
			if err != nil {
				return "", false, false
			}
			v, err := localTargetLabelValue(arg.(*syntax.BinaryExpr).Y)
			if err != nil || !strings.HasPrefix(v, ":") {
				return "", false, false
			}
			return v, skip, true
		}
	}
	return "", false, false
}

// hoistComponentManifests ensures that every un-skipped `fx_component_manifest`
// appears before the first un-skipped target in the same file that passes its
// label to GN `get_target_outputs(cm_label)`.
func hoistComponentManifests(f *syntax.File) error {
	manifestIdx := map[string]int{}
	for idx, stmt := range f.Stmts {
		call, ok := callOfStmt(stmt)
		if !ok {
			continue
		}
		fn, ok := call.Fn.(*syntax.Ident)
		if !ok || !isFxManifestRule(fn.Name) {
			continue
		}
		skip, err := hasSkipAnnotation(stmt)
		if err != nil {
			return err
		}
		if skip {
			continue
		}
		if name, ok := callStringAttr(call, "name"); ok && name != "" {
			manifestIdx[name] = idx
		}
	}
	if len(manifestIdx) == 0 {
		return nil
	}

	earliestConsumer := map[string]int{}
	for idx, stmt := range f.Stmts {
		call, ok := callOfStmt(stmt)
		if !ok {
			continue
		}
		fn, ok := call.Fn.(*syntax.Ident)
		if !ok {
			continue
		}
		var cmAttr string
		switch fn.Name {
		case "fx_component", internalFxTestComponentRule:
			cmAttr = "compiled_manifest"
		case "fuchsia_structured_config_rust_lib":
			cmAttr = "cm_label"
		default:
			continue
		}
		if bazelRuleToGNTemplate[fn.Name] == noGNEquivalent {
			continue
		}
		skip, err := hasSkipAnnotation(stmt)
		if err != nil {
			return err
		}
		if skip {
			continue
		}
		cmVal, ok := unskippedCallStringAttr(call, cmAttr)
		if !ok || !strings.HasPrefix(cmVal, ":") {
			continue
		}
		mName := strings.TrimPrefix(cmVal, ":")
		if _, exists := manifestIdx[mName]; !exists {
			continue
		}
		if prev, seen := earliestConsumer[mName]; !seen || idx < prev {
			earliestConsumer[mName] = idx
		}
	}

	hoistBefore := map[int][]syntax.Stmt{}
	hoisted := map[int]bool{}
	for idx, stmt := range f.Stmts {
		call, ok := callOfStmt(stmt)
		if !ok {
			continue
		}
		fn, ok := call.Fn.(*syntax.Ident)
		if !ok || !isFxManifestRule(fn.Name) {
			continue
		}
		name, ok := callStringAttr(call, "name")
		if !ok {
			continue
		}
		if consumerIdx, ok := earliestConsumer[name]; ok && consumerIdx < idx {
			hoistBefore[consumerIdx] = append(hoistBefore[consumerIdx], stmt)
			hoisted[idx] = true
		}
	}
	if len(hoisted) == 0 {
		return nil
	}

	newStmts := make([]syntax.Stmt, 0, len(f.Stmts))
	for idx, stmt := range f.Stmts {
		if before := hoistBefore[idx]; len(before) > 0 {
			newStmts = append(newStmts, before...)
		}
		if !hoisted[idx] {
			newStmts = append(newStmts, stmt)
		}
	}
	f.Stmts = newStmts
	return nil
}

// indexGNBinaryOutputNames maps each un-skipped binary target name defined in
// `f` to the executable `output_name` that its converted GN target produces in
// `$root_out_dir`.
func indexGNBinaryOutputNames(f *syntax.File) (map[string]string, error) {
	outputs := map[string]string{}
	for _, stmt := range f.Stmts {
		call, ok := callOfStmt(stmt)
		if !ok {
			continue
		}
		fn, ok := call.Fn.(*syntax.Ident)
		if !ok || bazelRuleToGNTemplate[fn.Name] == noGNEquivalent {
			continue
		}
		skip, err := hasSkipAnnotation(stmt)
		if err != nil {
			return nil, err
		}
		if skip {
			continue
		}
		name, ok := callStringAttr(call, "name")
		if !ok || name == "" {
			continue
		}

		rawOut, hasRawOut := rawOutputNameFromStmt(stmt, call)

		switch fn.Name {
		case "cc_binary", "fx_cc_binary", "cc_binary_host_tool", "idk_cc_binary_host_tool",
			"go_binary", "go_binary_host_tool", "idk_go_binary_host_tool":
			if hasRawOut {
				outputs[name] = rawOut
			} else {
				outputs[name] = name
			}
		case "rustc_binary", "rust_binary", "idk_rustc_binary_host_tool":
			outName := name
			if bn, ok := unskippedCallStringAttr(call, "binary_name"); ok {
				outName = bn
			} else if cn, ok := unskippedCallStringAttr(call, "crate_name"); ok {
				outName = cn
			}
			if hasRawOut {
				outName = rawOut
			}
			outputs[name] = outName
			if unskippedCallBoolTrue(call, "with_unit_tests") {
				outputs[name+"_test"] = strings.ReplaceAll(outName+"_bin_test", "-", "_")
			}
		case "rustc_test":
			outName := name
			if bn, ok := unskippedCallStringAttr(call, "binary_name"); ok {
				outName = bn
			} else if cn, ok := unskippedCallStringAttr(call, "crate_name"); ok {
				outName = cn
			}
			if hasRawOut {
				outName = rawOut
			}
			outputs[name] = strings.ReplaceAll(outName, "-", "_")
		case "rustc_library", "rust_library":
			if unskippedCallBoolTrue(call, "with_unit_tests") {
				crateName := name
				if cn, ok := unskippedCallStringAttr(call, "crate_name"); ok {
					crateName = cn
				}
				outputs[name+"_test"] = strings.ReplaceAll(crateName, "-", "_") + "_lib_test"
			}
		}
	}
	return outputs, nil
}

func rawOutputNameFromStmt(stmt syntax.Stmt, call *syntax.CallExpr) (string, bool) {
	start, _ := call.Span()
	rawAttrs, err := rawAttributesFromComments(start.Line, stmt.Comments(), call.Comments())
	if err != nil {
		return "", false
	}
	for _, raw := range rawAttrs {
		if m := rawOutputNameRE.FindStringSubmatch(raw); m != nil {
			return m[1], true
		}
	}
	return "", false
}

func unskippedCallStringAttr(call *syntax.CallExpr, attr string) (string, bool) {
	for _, arg := range call.Args {
		if skip, err := hasSkipAnnotation(arg); err != nil || skip {
			continue
		}
		if name, ok := attrName(arg); ok && name == attr {
			val, err := stringLiteralValue(arg.(*syntax.BinaryExpr).Y)
			return val, err == nil
		}
	}
	return "", false
}

func unskippedCallBoolTrue(call *syntax.CallExpr, attr string) bool {
	for _, arg := range call.Args {
		if skip, err := hasSkipAnnotation(arg); err != nil || skip {
			continue
		}
		if name, ok := attrName(arg); ok && name == attr {
			ident, ok := unwrapParenExpr(arg.(*syntax.BinaryExpr).Y).(*syntax.Ident)
			return ok && ident.Name == "True"
		}
	}
	return false
}

func resolveComponentDepsExpr(
	expr syntax.Expr,
	compName string,
	pkgBins map[string]*packagedBinaryRecord,
	gnBinOutputs map[string]string,
	varStmts map[string]*syntax.AssignStmt,
	visitedVars map[string]bool,
) (syntax.Expr, error) {
	switch v := expr.(type) {
	case *syntax.ParenExpr:
		inner, err := resolveComponentDepsExpr(v.X, compName, pkgBins, gnBinOutputs, varStmts, visitedVars)
		if err != nil {
			return nil, err
		}
		v.X = inner
		return v, nil
	case *syntax.ListExpr:
		for i, elem := range v.List {
			skip, err := hasSkipAnnotation(elem)
			if err != nil {
				return nil, err
			}
			if skip {
				continue
			}
			rewritten, err := resolveComponentDepsExpr(elem, compName, pkgBins, gnBinOutputs, varStmts, visitedVars)
			if err != nil {
				return nil, err
			}
			v.List[i] = rewritten
		}
		return v, nil
	case *syntax.BinaryExpr:
		if v.Op == syntax.PLUS {
			lhs, err := resolveComponentDepsExpr(v.X, compName, pkgBins, gnBinOutputs, varStmts, visitedVars)
			if err != nil {
				return nil, err
			}
			rhs, err := resolveComponentDepsExpr(v.Y, compName, pkgBins, gnBinOutputs, varStmts, visitedVars)
			if err != nil {
				return nil, err
			}
			v.X = lhs
			v.Y = rhs
		}
		return v, nil
	case *syntax.CondExpr:
		tBranch, err := resolveComponentDepsExpr(v.True, compName, pkgBins, gnBinOutputs, varStmts, visitedVars)
		if err != nil {
			return nil, err
		}
		fBranch, err := resolveComponentDepsExpr(v.False, compName, pkgBins, gnBinOutputs, varStmts, visitedVars)
		if err != nil {
			return nil, err
		}
		v.True = tBranch
		v.False = fBranch
		return v, nil
	case *syntax.CallExpr:
		if isSelectCall(v) && len(v.Args) == 1 {
			if dict, ok := unwrapParenExpr(v.Args[0]).(*syntax.DictExpr); ok {
				for _, entry := range dict.List {
					kv, ok := entry.(*syntax.DictEntry)
					if !ok {
						continue
					}
					rewritten, err := resolveComponentDepsExpr(kv.Value, compName, pkgBins, gnBinOutputs, varStmts, visitedVars)
					if err != nil {
						return nil, err
					}
					kv.Value = rewritten
				}
			}
		}
		return v, nil
	case *syntax.Ident:
		if assign, ok := varStmts[v.Name]; ok && !visitedVars[v.Name] {
			visitedVars[v.Name] = true
			rewritten, err := resolveComponentDepsExpr(assign.RHS, compName, pkgBins, gnBinOutputs, varStmts, visitedVars)
			if err != nil {
				return nil, err
			}
			assign.RHS = rewritten
		}
		return v, nil
	case *syntax.Literal:
		if _, hasOverwrite := overwrittenPath(v); hasOverwrite {
			return v, nil
		}
		label, ok := v.Value.(string)
		if !ok || !strings.HasPrefix(label, ":") {
			return v, nil
		}
		targetName := strings.TrimPrefix(label, ":")
		rec, isPkgBin := pkgBins[targetName]
		if !isPkgBin {
			return v, nil
		}
		resolvedLabel, err := resolvePackagedBinaryRecord(rec, targetName, compName, gnBinOutputs)
		if err != nil {
			return nil, err
		}
		if resolvedLabel == label {
			return v, nil
		}
		rewritten := *v
		rewritten.Value = resolvedLabel
		rewritten.Raw = gnQuote(resolvedLabel)
		return &rewritten, nil
	default:
		return expr, nil
	}
}

func resolvePackagedBinaryRecord(
	rec *packagedBinaryRecord,
	pkgBinName string,
	compName string,
	gnBinOutputs map[string]string,
) (string, error) {
	if rec.emittedName {
		return ":" + pkgBinName, nil
	}

	var binaryLabel, binaryName string
	installRoot := "bin/"
	var testonlyArg *syntax.BinaryExpr
	var trailingArgs []syntax.Expr

	for _, arg := range rec.call.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			return "", fmt.Errorf("fx_packaged_binary %q only accepts keyword arguments, got %T", pkgBinName, arg)
		}
		shouldSkip, err := hasSkipAnnotation(binaryExpr)
		if err != nil {
			return "", fmt.Errorf("failed to check skip annotation: %v", err)
		}
		if shouldSkip {
			continue
		}
		ident, ok := binaryExpr.X.(*syntax.Ident)
		if !ok {
			return "", fmt.Errorf("expected identifier for argument name in fx_packaged_binary %q, got %T", pkgBinName, binaryExpr.X)
		}
		if !allowedFxPackagedBinaryAttrs[ident.Name] {
			return "", fmt.Errorf("bazel2gn doesn't support attribute %q of fx_packaged_binary %q; use # @bazel2gn:skip on fx_component %q and write the GN copy by hand", ident.Name, pkgBinName, compName)
		}
		switch ident.Name {
		case "name":
			// Handled via pkgBinName.
		case "binary":
			binaryLabel, err = stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return "", fmt.Errorf("fx_packaged_binary %q binary: %v", pkgBinName, err)
			}
		case "binary_name":
			binaryName, err = stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return "", fmt.Errorf("fx_packaged_binary %q binary_name: %v", pkgBinName, err)
			}
		case "install_root":
			installRoot, err = stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return "", fmt.Errorf("fx_packaged_binary %q install_root: %v", pkgBinName, err)
			}
		case "testonly":
			testonlyArg = binaryExpr
		case "visibility", "target_compatible_with":
			trailingArgs = append(trailingArgs, binaryExpr)
		case "tags":
			// Bazel tags on fx_packaged_binary (e.g. "manual") have no GN meaning.
		}
	}

	if binaryLabel == "" {
		return "", fmt.Errorf("missing `binary` attribute in fx_packaged_binary %q", pkgBinName)
	}
	if binaryName == "" {
		return "", fmt.Errorf("missing `binary_name` attribute in fx_packaged_binary %q", pkgBinName)
	}
	if !strings.HasPrefix(binaryLabel, ":") {
		return "", fmt.Errorf("fx_packaged_binary %q binary %q (used by fx_component %q) must be a target in the same BUILD.bazel (\":<bin>\"); use # @bazel2gn:skip on fx_component %q and write the GN copy by hand", pkgBinName, binaryLabel, compName, compName)
	}

	binTarget := strings.TrimPrefix(binaryLabel, ":")
	gnOutName, ok := gnBinOutputs[binTarget]
	if !ok {
		return "", fmt.Errorf("fx_packaged_binary %q binary %q (used by fx_component %q) does not refer to an unskipped binary target in the same BUILD.bazel; use # @bazel2gn:skip on fx_component %q and write the GN copy by hand", pkgBinName, binaryLabel, compName, compName)
	}

	dest := installRoot + binaryName
	if dest == "bin/"+gnOutName {
		return ":" + binTarget, nil
	}

	if rec.skipped {
		return "", fmt.Errorf("fx_packaged_binary %q is referenced by fx_component %q and requires a GN renamed_binary target (dest %q != default %q), so it cannot have # @bazel2gn:skip", pkgBinName, compName, dest, "bin/"+gnOutName)
	}
	if hasRawAttributeAnnotation(rec.stmt) {
		return "", errors.New("raw_attribute annotation is not supported on fx_packaged_binary")
	}

	newArgs := []syntax.Expr{attrExpr("name", stringLiteral(pkgBinName))}
	if testonlyArg != nil {
		newArgs = append(newArgs, testonlyArg)
	}
	newArgs = append(newArgs,
		attrExpr("source", stringLiteral("$root_out_dir/"+gnOutName)),
		attrExpr("source_deps", &syntax.ListExpr{List: []syntax.Expr{stringLiteral(":" + binTarget)}}),
		attrExpr("dest", stringLiteral(dest)),
	)
	newArgs = append(newArgs, trailingArgs...)
	rec.call.Fn = &syntax.Ident{Name: internalRenamedBinaryRule}
	rec.call.Args = newArgs
	rec.emittedName = true
	return ":" + pkgBinName, nil
}
