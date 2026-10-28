// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn

import (
	"errors"
	"fmt"
	"slices"
	"strings"

	"go.starlark.net/syntax"
)

const (
	// Internal keyword arguments attached by `resolveLinuxResourceTargets` when
	// a same-file binary target referenced by `linux_resource` produces a GN
	// output file whose name differs from its target name. `$` is invalid in
	// Starlark identifiers, so user `BUILD.bazel` files can never pass these.
	internalBinOutputNameAttr       = "$bin_output_name"
	internalArch32BinOutputNameAttr = "$arch32_bin_output_name"
)

var (
	allowedLinuxResourceAttrs = map[string]bool{
		"name":                          true,
		"src":                           true,
		"dest":                          true,
		"arch32_src":                    true,
		"arch32_dest":                   true,
		"testonly":                      true,
		"visibility":                    true,
		"tags":                          true,
		"target_compatible_with":        true,
		internalBinOutputNameAttr:       true,
		internalArch32BinOutputNameAttr: true,
	}

	// Select conditions accepted on `arch32_src` (and `arch32_dest`) to guard
	// 32-bit ARM Linux resources on arm64 Fuchsia builds.
	arm64SelectConditions = map[string]bool{
		"@platforms//cpu:arm64":                    true,
		"@platforms//cpu:aarch64":                  true,
		"//build/bazel/platforms:is_fuchsia_arm64": true,
	}

	osTargetConstraints = map[string]bool{
		"@platforms//os:fuchsia":                       true,
		"//build/bazel/platforms:linux_flavor_starnix": true,
		"HOST_CONSTRAINTS":                             true,
		"HOST_OS_CONSTRAINTS":                          true,
	}
)

func isLinuxResourceBinaryRule(rule string) bool {
	switch rule {
	case "cc_binary", "fx_cc_binary", "rustc_binary", "rust_binary":
		return true
	default:
		return false
	}
}

func hasRawOutputNameAttr(rawAttrs []string) bool {
	for _, raw := range rawAttrs {
		if strings.HasPrefix(strings.TrimSpace(raw), "output_name") {
			return true
		}
	}
	return false
}

// resolveLinuxResourceTargets prepares a parsed `BUILD.bazel` file so Starnix
// `linux_resource()` targets and their same-file Linux binaries convert cleanly
// to GN:
//
//  1. Records `bin_output_name` / `arch32_bin_output_name` on each un-skipped
//     `linux_resource()` whose `src` / `arch32_src` refers to an un-skipped
//     binary target in the same file with a non-default GN output filename
//     (for example, `rustc_binary` with `crate_name` / `binary_name`, or
//     `fx_cc_binary(linkshared = True)`).
//  2. Adds `"//build/bazel/platforms:linux_flavor_starnix"` to
//     `target_compatible_with` (wrapping the generated GN target in
//     `if (is_linux && !is_host) { ... }`) on any un-skipped binary in the same
//     file that is referenced by `linux_resource()` and has no other GN
//     consumers in the file, so Linux-only `configs` / `sources` are not
//     evaluated in Fuchsia or host toolchains.
func resolveLinuxResourceTargets(f *syntax.File) error {
	gnBinOutputs, err := indexGNBinaryOutputNames(f)
	if err != nil {
		return err
	}

	linuxResBins := map[string]bool{}
	for _, stmt := range f.Stmts {
		call, ok := callOfStmt(stmt)
		if !ok {
			continue
		}
		fn, ok := call.Fn.(*syntax.Ident)
		if !ok || fn.Name != "linux_resource" {
			continue
		}
		stmtSkipped, err := hasSkipAnnotation(stmt)
		if err != nil {
			return err
		}

		var extraArgs []syntax.Expr
		for _, arg := range call.Args {
			argSkipped, err := hasSkipAnnotation(arg)
			if err != nil {
				return err
			}
			if argSkipped {
				continue
			}
			name, ok := attrName(arg)
			if !ok {
				continue
			}
			kv := arg.(*syntax.BinaryExpr)
			switch name {
			case "src":
				if binTarget, ok := localTargetFromLinuxResourceExpr(kv, kv.Y); ok {
					linuxResBins[binTarget] = true
					if !stmtSkipped {
						if gnOut, exists := gnBinOutputs[binTarget]; exists && gnOut != binTarget {
							extraArgs = append(extraArgs, attrExpr(internalBinOutputNameAttr, stringLiteral(gnOut)))
						}
					}
				}
			case "arch32_src":
				if binTarget, ok := localTargetFromArch32Expr(kv, kv.Y); ok {
					linuxResBins[binTarget] = true
					if !stmtSkipped {
						if gnOut, exists := gnBinOutputs[binTarget]; exists && gnOut != binTarget {
							extraArgs = append(extraArgs, attrExpr(internalArch32BinOutputNameAttr, stringLiteral(gnOut)))
						}
					}
				}
			}
		}
		if len(extraArgs) > 0 {
			call.Args = append(call.Args, extraArgs...)
		}
	}

	if len(linuxResBins) == 0 {
		return nil
	}

	otherGNConsumers := map[string]bool{}
	for _, stmt := range f.Stmts {
		stmtSkipped, err := hasSkipAnnotation(stmt)
		if err != nil {
			return err
		}
		if stmtSkipped {
			continue
		}
		if assign, ok := stmt.(*syntax.AssignStmt); ok {
			walkLocalLabelRefs(assign.RHS, func(target string) {
				otherGNConsumers[target] = true
			})
			continue
		}
		call, ok := callOfStmt(stmt)
		if !ok {
			continue
		}
		fn, ok := call.Fn.(*syntax.Ident)
		if !ok {
			continue
		}
		if fn.Name == "linux_resource" || fn.Name == "package" || bazelRuleToGNTemplate[fn.Name] == noGNEquivalent {
			continue
		}
		var binName string
		if isLinuxResourceBinaryRule(fn.Name) {
			binName, _ = callStringAttr(call, "name")
		}
		for _, arg := range call.Args {
			if skip, err := hasSkipAnnotation(arg); err != nil || skip {
				continue
			}
			attr, ok := attrName(arg)
			if !ok || attr == "name" || attr == "visibility" {
				continue
			}
			kv := arg.(*syntax.BinaryExpr)
			if (attr == "with_unit_tests" || attr == "with_host_unit_tests") && binName != "" {
				if rhsIdent, ok := unwrapParenExpr(kv.Y).(*syntax.Ident); !ok || rhsIdent.Name != "False" {
					otherGNConsumers[binName] = true
				}
			}
			walkLocalLabelRefs(kv.Y, func(target string) {
				otherGNConsumers[target] = true
			})
		}
	}

	for _, stmt := range f.Stmts {
		stmtSkipped, err := hasSkipAnnotation(stmt)
		if err != nil || stmtSkipped {
			continue
		}
		call, ok := callOfStmt(stmt)
		if !ok {
			continue
		}
		fn, ok := call.Fn.(*syntax.Ident)
		if !ok || !isLinuxResourceBinaryRule(fn.Name) {
			continue
		}
		binName, ok := callStringAttr(call, "name")
		if !ok || !linuxResBins[binName] || otherGNConsumers[binName] {
			continue
		}
		ensureLinuxTargetCompatibleWith(call)
	}

	return nil
}

func stripLabelToolchain(label string) string {
	if strings.HasSuffix(label, ")") {
		if idx := strings.LastIndexByte(label, '('); idx >= 0 {
			return label[:idx]
		}
	}
	return label
}

func localTargetFromLinuxResourceExpr(owner syntax.Node, expr syntax.Expr) (string, bool) {
	if _, hasRaw := overwrittenRawOnAny(owner, expr); hasRaw {
		return "", false
	}
	if path, hasPath := overwrittenPathOnAny(owner, expr); hasPath {
		if target, ok := strings.CutPrefix(stripLabelToolchain(path), ":"); ok && target != "" {
			return target, true
		}
		return "", false
	}
	val, err := stringLiteralValue(expr)
	if err != nil {
		return "", false
	}
	target, ok := strings.CutPrefix(stripLabelToolchain(val), ":")
	return target, ok && target != ""
}

func localTargetFromArch32Expr(owner syntax.Node, expr syntax.Expr) (string, bool) {
	unwrapped := unwrapParenExpr(expr)
	if isSelectCall(unwrapped) {
		sel := unwrapped.(*syntax.CallExpr)
		if len(sel.Args) == 0 {
			return "", false
		}
		dict, ok := unwrapParenExpr(sel.Args[0]).(*syntax.DictExpr)
		if !ok {
			return "", false
		}
		for _, item := range dict.List {
			entry, ok := item.(*syntax.DictEntry)
			if !ok {
				continue
			}
			key, err := stringLiteralValue(entry.Key)
			if err != nil || !arm64SelectConditions[key] {
				continue
			}
			return localTargetFromLinuxResourceExpr(entry, entry.Value)
		}
		return "", false
	}
	return localTargetFromLinuxResourceExpr(owner, unwrapped)
}

func walkLocalLabelRefs(root syntax.Node, visit func(target string)) {
	if root == nil {
		return
	}
	syntax.Walk(root, func(n syntax.Node) bool {
		if n == nil {
			return false
		}
		if skip, err := hasSkipAnnotation(n); err == nil && skip {
			return false
		}
		lit, ok := n.(*syntax.Literal)
		if !ok {
			return true
		}
		label := ""
		if overwritten, hasOverwrite := overwrittenPath(lit); hasOverwrite {
			label = overwritten
		} else if s, isStr := lit.Value.(string); isStr {
			label = s
		}
		label = stripLabelToolchain(label)
		if target, ok := strings.CutPrefix(label, ":"); ok && target != "" {
			visit(target)
			if base, hasTest := strings.CutSuffix(target, "_test"); hasTest && base != "" {
				visit(base)
			}
		}
		return true
	})
}

func ensureLinuxTargetCompatibleWith(call *syntax.CallExpr) {
	for _, arg := range call.Args {
		if skip, err := hasSkipAnnotation(arg); err != nil || skip {
			continue
		}
		attr, ok := attrName(arg)
		if !ok || attr != "target_compatible_with" {
			continue
		}
		kv := arg.(*syntax.BinaryExpr)
		list, ok := unwrapParenExpr(kv.Y).(*syntax.ListExpr)
		if !ok {
			return
		}
		for _, elem := range list.List {
			switch v := unwrapParenExpr(elem).(type) {
			case *syntax.Literal:
				if s, isStr := v.Value.(string); isStr && osTargetConstraints[s] {
					return
				}
			case *syntax.Ident:
				if osTargetConstraints[v.Name] {
					return
				}
			}
		}
		list.List = append([]syntax.Expr{stringLiteral("//build/bazel/platforms:linux_flavor_starnix")}, slices.Clone(list.List)...)
		return
	}
	call.Args = append(call.Args, attrExpr(
		"target_compatible_with",
		&syntax.ListExpr{List: []syntax.Expr{stringLiteral("//build/bazel/platforms:linux_flavor_starnix")}},
	))
}

// linuxResourceToGN converts a Bazel `linux_resource()` rule call to a GN
// `linux_resource()` target (defined in //src/starnix/build/linux_resource.gni).
func linuxResourceToGN(call *syntax.CallExpr) ([]string, error) {
	var name, binOutputName, arch32BinOutputName string
	var srcArg, destArg, arch32SrcArg, arch32DestArg *syntax.BinaryExpr
	var testonlyArg, tagsArg, visArg *syntax.BinaryExpr
	var wrappingConditions []string

	for _, arg := range call.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			return nil, fmt.Errorf("linux_resource only accepts keyword arguments, got %T", arg)
		}
		shouldSkip, err := hasSkipAnnotation(binaryExpr)
		if err != nil {
			return nil, fmt.Errorf("failed to check skip annotation: %v", err)
		}
		if shouldSkip {
			continue
		}
		if hasRawAttributeAnnotation(binaryExpr) {
			return nil, errors.New("found raw_attribute annotation on attribute, it should be above the target definition to take effect")
		}
		ident, ok := binaryExpr.X.(*syntax.Ident)
		if !ok {
			return nil, fmt.Errorf("expected identifier for argument name, got %T", binaryExpr.X)
		}
		if !allowedLinuxResourceAttrs[ident.Name] {
			return nil, fmt.Errorf("bazel2gn doesn't support attribute %q of linux_resource; use # @bazel2gn:skip and write the GN copy by hand", ident.Name)
		}

		switch ident.Name {
		case "name":
			name, err = stringLiteralValue(binaryExpr.Y)
			if err != nil {
				return nil, fmt.Errorf("linux_resource name: %v", err)
			}
		case "src":
			srcArg = binaryExpr
		case "dest":
			destArg = binaryExpr
		case "arch32_src":
			arch32SrcArg = binaryExpr
		case "arch32_dest":
			arch32DestArg = binaryExpr
		case internalBinOutputNameAttr:
			binOutputName, _ = stringLiteralValue(binaryExpr.Y)
		case internalArch32BinOutputNameAttr:
			arch32BinOutputName, _ = stringLiteralValue(binaryExpr.Y)
		case "testonly":
			testonlyArg = binaryExpr
		case "tags":
			tagsArg = binaryExpr
		case "visibility":
			visArg = binaryExpr
		case "target_compatible_with":
			wrappingConditions, err = targetCompatibleWithToGNConditions(binaryExpr.Y)
			if err != nil {
				return nil, fmt.Errorf("converting Bazel target_compatible_with to GN conditions: %v", err)
			}
		}
	}

	if name == "" {
		return nil, errors.New("missing `name` attribute in linux_resource")
	}

	hasSrc := false
	var srcGN, destVal string
	if srcArg != nil && !isNoneIdent(srcArg.Y) {
		var err error
		srcGN, err = linuxResourceLabelToGN(srcArg, srcArg.Y, "src", name)
		if err != nil {
			return nil, err
		}
		hasSrc = true
	}
	if hasSrc {
		if destArg == nil {
			return nil, fmt.Errorf("missing `dest` attribute in linux_resource %q", name)
		}
		var err error
		destVal, err = validateLinuxResourceDest(destArg.Y, "dest", name)
		if err != nil {
			return nil, err
		}
	} else if destArg != nil {
		return nil, fmt.Errorf("`dest` attribute in linux_resource %q requires `src`", name)
	}

	hasArch32 := false
	arch32Conditional := false
	var arch32SrcGN, arch32DestVal string
	if arch32SrcArg != nil && !isNoneIdent(arch32SrcArg.Y) {
		unwrapped := unwrapParenExpr(arch32SrcArg.Y)
		if isSelectCall(unwrapped) {
			var err error
			arch32SrcGN, hasArch32, err = parseArch32SelectLabelToGN(unwrapped.(*syntax.CallExpr), name)
			if err != nil {
				return nil, err
			}
			arch32Conditional = true
		} else {
			var err error
			arch32SrcGN, err = linuxResourceLabelToGN(arch32SrcArg, unwrapped, "arch32_src", name)
			if err != nil {
				return nil, err
			}
			hasArch32 = true
		}
	}
	if hasArch32 {
		if arch32DestArg == nil {
			return nil, fmt.Errorf("missing `arch32_dest` attribute in linux_resource %q when `arch32_src` is set", name)
		}
		var err error
		arch32DestVal, err = parseArch32Dest(arch32DestArg.Y, name)
		if err != nil {
			return nil, err
		}
	} else if arch32DestArg != nil && arch32SrcArg == nil {
		return nil, fmt.Errorf("`arch32_dest` attribute in linux_resource %q requires `arch32_src`", name)
	}

	if !hasSrc && !hasArch32 {
		return nil, fmt.Errorf("linux_resource %q requires `src` or `arch32_src`", name)
	}

	ret := []string{fmt.Sprintf("linux_resource(%s) {", gnQuote(name))}

	if testonlyArg != nil {
		lines, err := attrAssignmentToGN(testonlyArg, "linux_resource", nil, false, false)
		if err != nil {
			return nil, fmt.Errorf("converting linux_resource testonly: %v", err)
		}
		ret = append(ret, indent(lines, 1)...)
	}

	if hasSrc {
		ret = append(ret, indent([]string{fmt.Sprintf("src = %s", srcGN)}, 1)...)
		if binOutputName != "" {
			ret = append(ret, indent([]string{fmt.Sprintf("bin_output_name = %s", gnQuote(binOutputName))}, 1)...)
		}
		ret = append(ret, indent([]string{fmt.Sprintf("dest = %s", gnQuote(destVal))}, 1)...)
	}

	if hasArch32 {
		arch32Lines := []string{fmt.Sprintf("arch32_src = %s", arch32SrcGN)}
		if arch32BinOutputName != "" {
			arch32Lines = append(arch32Lines, fmt.Sprintf("arch32_bin_output_name = %s", gnQuote(arch32BinOutputName)))
		}
		arch32Lines = append(arch32Lines, fmt.Sprintf("arch32_dest = %s", gnQuote(arch32DestVal)))
		if arch32Conditional {
			ret = append(ret, indent([]string{`if (target_cpu == "arm64") {`}, 1)...)
			ret = append(ret, indent(arch32Lines, 2)...)
			ret = append(ret, indent([]string{"}"}, 1)...)
		} else {
			ret = append(ret, indent(arch32Lines, 1)...)
		}
	}

	if tagsArg != nil {
		lines, err := attrAssignmentToGN(tagsArg, "linux_resource", nil, false, false)
		if err != nil {
			return nil, fmt.Errorf("converting linux_resource tags: %v", err)
		}
		ret = append(ret, indent(lines, 1)...)
	}

	if visArg != nil {
		lines, err := attrAssignmentToGN(visArg, "linux_resource", nil, false, false)
		if err != nil {
			return nil, fmt.Errorf("converting linux_resource visibility: %v", err)
		}
		ret = append(ret, indent(lines, 1)...)
	}

	ret = append(ret, "}")
	if len(wrappingConditions) > 0 {
		ret = append([]string{
			fmt.Sprintf("if (%s) {", strings.Join(wrappingConditions, " && ")),
		}, indent(ret, 1)...)
		ret = append(ret, "}")
	}
	return ret, nil
}

func isNoneIdent(expr syntax.Expr) bool {
	ident, ok := unwrapParenExpr(expr).(*syntax.Ident)
	return ok && ident.Name == "None"
}

func overwrittenPathOnAny(nodes ...syntax.Node) (string, bool) {
	for _, n := range nodes {
		if n == nil || n.Comments() == nil {
			continue
		}
		for _, c := range n.Comments().Suffix {
			if after, ok := strings.CutPrefix(c.Text, pathOverwriteAnnotationPrefix); ok {
				return strings.TrimSpace(after), true
			}
		}
	}
	return "", false
}

func overwrittenRawOnAny(nodes ...syntax.Node) (string, bool) {
	for _, n := range nodes {
		if raw, ok := overwrittenRaw(n); ok {
			return raw, true
		}
	}
	return "", false
}

func linuxResourceLabelToGN(owner syntax.Node, expr syntax.Expr, attr, targetName string) (string, error) {
	if raw, ok := overwrittenRawOnAny(owner, expr); ok {
		return raw, nil
	}
	if path, ok := overwrittenPathOnAny(owner, expr); ok {
		return gnQuote(path), nil
	}
	lit, ok := unwrapParenExpr(expr).(*syntax.Literal)
	if !ok {
		return "", fmt.Errorf("linux_resource %q %s must be a string literal label, got %T", targetName, attr, expr)
	}
	if _, isStr := lit.Value.(string); !isStr {
		return "", fmt.Errorf("linux_resource %q %s must be a string literal label, got %s", targetName, attr, strings.TrimSpace(lit.Raw))
	}
	cloned := *lit
	transformed, err := bazelDepToGN(&cloned)
	if err != nil {
		return "", fmt.Errorf("linux_resource %q %s: %v", targetName, attr, err)
	}
	return transformed.(*syntax.Literal).Raw, nil
}

func validateLinuxResourceDest(expr syntax.Expr, attr, targetName string) (string, error) {
	dest, err := stringLiteralValue(expr)
	if err != nil {
		return "", fmt.Errorf("linux_resource %q %s: %v", targetName, attr, err)
	}
	if dest == "" {
		return "", fmt.Errorf("linux_resource %q %s must not be empty", targetName, attr)
	}
	if strings.HasPrefix(dest, "/") {
		return "", fmt.Errorf("linux_resource %q %s %q must not start with \"/\"", targetName, attr, dest)
	}
	return dest, nil
}

func parseArch32SelectLabelToGN(sel *syntax.CallExpr, targetName string) (string, bool, error) {
	if len(sel.Args) != 1 {
		return "", false, fmt.Errorf("linux_resource %q arch32_src select() must take a single dictionary argument", targetName)
	}
	dict, ok := unwrapParenExpr(sel.Args[0]).(*syntax.DictExpr)
	if !ok {
		return "", false, fmt.Errorf("linux_resource %q arch32_src select() argument must be a dictionary literal, got %T", targetName, sel.Args[0])
	}
	if len(dict.List) != 2 {
		return "", false, fmt.Errorf("linux_resource %q arch32_src select() must have an arm64 branch and %q: None", targetName, defaultBazelSelectCondition)
	}

	var arm64Entry *syntax.DictEntry
	hasDefaultNone := false
	for _, item := range dict.List {
		entry, ok := item.(*syntax.DictEntry)
		if !ok {
			return "", false, fmt.Errorf("unexpected dictionary entry %T in linux_resource %q arch32_src", item, targetName)
		}
		cond, err := stringLiteralValue(entry.Key)
		if err != nil {
			return "", false, fmt.Errorf("linux_resource %q arch32_src select() key: %v", targetName, err)
		}
		switch {
		case cond == defaultBazelSelectCondition:
			if !isNoneIdent(entry.Value) {
				return "", false, fmt.Errorf("linux_resource %q arch32_src select() %q branch must be None", targetName, defaultBazelSelectCondition)
			}
			hasDefaultNone = true
		case arm64SelectConditions[cond]:
			if arm64Entry != nil {
				return "", false, fmt.Errorf("linux_resource %q arch32_src select() has multiple arm64 branches", targetName)
			}
			arm64Entry = entry
		default:
			return "", false, fmt.Errorf("unsupported condition %q in linux_resource %q arch32_src select(); expected \"@platforms//cpu:arm64\" and %q: None", cond, targetName, defaultBazelSelectCondition)
		}
	}
	if !hasDefaultNone || arm64Entry == nil {
		return "", false, fmt.Errorf("linux_resource %q arch32_src select() must have an arm64 branch and %q: None", targetName, defaultBazelSelectCondition)
	}
	if isNoneIdent(arm64Entry.Value) {
		return "", false, nil
	}
	gnLabel, err := linuxResourceLabelToGN(arm64Entry, arm64Entry.Value, "arch32_src", targetName)
	if err != nil {
		return "", false, err
	}
	return gnLabel, true, nil
}

func parseArch32Dest(expr syntax.Expr, targetName string) (string, error) {
	unwrapped := unwrapParenExpr(expr)
	if !isSelectCall(unwrapped) {
		return validateLinuxResourceDest(unwrapped, "arch32_dest", targetName)
	}
	sel := unwrapped.(*syntax.CallExpr)
	if len(sel.Args) != 1 {
		return "", fmt.Errorf("linux_resource %q arch32_dest select() must take a single dictionary argument", targetName)
	}
	dict, ok := unwrapParenExpr(sel.Args[0]).(*syntax.DictExpr)
	if !ok || len(dict.List) != 2 {
		return "", fmt.Errorf("linux_resource %q arch32_dest select() must have an arm64 branch and %q", targetName, defaultBazelSelectCondition)
	}
	var arm64Dest string
	hasDefault := false
	for _, item := range dict.List {
		entry, ok := item.(*syntax.DictEntry)
		if !ok {
			return "", fmt.Errorf("unexpected dictionary entry %T in linux_resource %q arch32_dest", item, targetName)
		}
		cond, err := stringLiteralValue(entry.Key)
		if err != nil {
			return "", fmt.Errorf("linux_resource %q arch32_dest select() key: %v", targetName, err)
		}
		switch {
		case cond == defaultBazelSelectCondition:
			if !isNoneIdent(entry.Value) {
				defVal, err := stringLiteralValue(entry.Value)
				if err != nil || defVal != "" {
					return "", fmt.Errorf("linux_resource %q arch32_dest select() %q branch must be \"\" or None", targetName, defaultBazelSelectCondition)
				}
			}
			hasDefault = true
		case arm64SelectConditions[cond]:
			if arm64Dest != "" {
				return "", fmt.Errorf("linux_resource %q arch32_dest select() has multiple arm64 branches", targetName)
			}
			arm64Dest, err = validateLinuxResourceDest(entry.Value, "arch32_dest", targetName)
			if err != nil {
				return "", err
			}
		default:
			return "", fmt.Errorf("unsupported condition %q in linux_resource %q arch32_dest select()", cond, targetName)
		}
	}
	if !hasDefault || arm64Dest == "" {
		return "", fmt.Errorf("linux_resource %q arch32_dest select() must have an arm64 branch and %q", targetName, defaultBazelSelectCondition)
	}
	return arm64Dest, nil
}
