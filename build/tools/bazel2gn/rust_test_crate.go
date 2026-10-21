// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn

import (
	"fmt"
	"slices"
	"strings"

	"go.starlark.net/syntax"
)

// rustTestRules are the Bazel Rust test rules that can test the crate of
// another target through the `crate` attribute.
var rustTestRules = map[string]bool{
	"rustc_test":      true,
	"host_rustc_test": true,
}

// procMacroRules are the Bazel rules for proc-macro crates.
var procMacroRules = map[string]bool{
	"rustc_proc_macro": true,
	"rust_proc_macro":  true,
}

// crateAttrs are attributes that a Rust test with `crate` takes from the
// target named by `crate` instead of from the test, like rules_rust's
// `rust_test()` does. rules_rust ignores the test's own `edition`.
var crateAttrs = map[string]bool{
	"srcs":       true,
	"crate_root": true,
	"edition":    true,
	"rustc_env":  true,
}

// crateListAttrs are list attributes that a Rust test with `crate` gets from
// both the crate and the test itself, like rules_rust's `rust_test()` does.
var crateListAttrs = map[string]bool{
	"deps":            true,
	"proc_macro_deps": true,
	"link_deps":       true,
	"compile_data":    true,
}

// inlineRustTestCrates rewrites Rust tests that use `crate = ":foo"` to
// instead set the attributes they inherit from `foo` explicitly.
//
// A Bazel Rust test can compile the sources of another Rust target with
// `crate`, but GN's `rustc_test()` has no such attribute, so the GN copy of
// the test needs the sources, deps and other attributes of that target. Only
// targets defined in the same file are supported.
func inlineRustTestCrates(f *syntax.File) error {
	targets := map[string]*syntax.CallExpr{}
	for _, stmt := range f.Stmts {
		call, ok := callOfStmt(stmt)
		if !ok {
			continue
		}
		name, ok := callStringAttr(call, "name")
		if !ok {
			continue
		}
		if fn, ok := call.Fn.(*syntax.Ident); ok && fn.Name == "ffx_plugin" {
			if execCrate := ffxPluginExecCrate(call, name); execCrate != nil {
				targets[name] = execCrate
			}
			targets[name+"_args"] = ffxPluginArgsCrate(call, name)
			continue
		}
		targets[name] = call
	}

	for _, stmt := range f.Stmts {
		call, ok := callOfStmt(stmt)
		if !ok {
			continue
		}
		fn, ok := call.Fn.(*syntax.Ident)
		if !ok || !rustTestRules[fn.Name] || bazelRuleToGNTemplate[fn.Name] == noGNEquivalent {
			continue
		}
		if skip, err := hasSkipAnnotation(stmt); err != nil || skip {
			continue
		}
		crateIdx := -1
		for i, arg := range call.Args {
			if skip, err := hasSkipAnnotation(arg); err != nil || skip {
				continue
			}
			if attr, ok := attrName(arg); ok && attr == "crate" {
				crateIdx = i
			}
		}
		if crateIdx < 0 {
			continue
		}
		crate, err := stringLiteralValue(call.Args[crateIdx].(*syntax.BinaryExpr).Y)
		if err != nil {
			return fmt.Errorf("crate attribute of %s: %v", fn.Name, err)
		}
		if !strings.HasPrefix(crate, ":") {
			return fmt.Errorf("bazel2gn only supports a crate attribute that names a target in the same BUILD.bazel, got %q; use # @bazel2gn:skip and write the GN copy by hand", crate)
		}
		crateCall, ok := targets[strings.TrimPrefix(crate, ":")]
		if !ok {
			return fmt.Errorf("crate %q of %s is not defined in the same BUILD.bazel", crate, fn.Name)
		}

		testArgs := map[string]*syntax.BinaryExpr{}
		for _, arg := range call.Args {
			if skip, err := hasSkipAnnotation(arg); err != nil || skip {
				continue
			}
			if attr, ok := attrName(arg); ok {
				testArgs[attr] = arg.(*syntax.BinaryExpr)
			}
		}

		var newArgs []syntax.Expr
		for _, arg := range crateCall.Args {
			attr, ok := attrName(arg)
			if !ok || !(crateAttrs[attr] || crateListAttrs[attr]) {
				continue
			}
			crateArg := arg.(*syntax.BinaryExpr)
			if skip, err := hasSkipAnnotation(crateArg); err != nil || skip {
				continue
			}
			testArg, inTest := testArgs[attr]
			switch {
			case !inTest:
				newArgs = append(newArgs, crateArg)
			case crateListAttrs[attr]:
				merged := *testArg
				merged.Y = &syntax.BinaryExpr{X: crateArg.Y, Op: syntax.PLUS, Y: testArg.Y}
				newArgs = append(newArgs, &merged)
			case attr == "rustc_env":
				return fmt.Errorf("bazel2gn doesn't support setting rustc_env on both %s and its crate %q; use # @bazel2gn:skip and write the GN copy by hand", fn.Name, crate)
			default:
				newArgs = append(newArgs, crateArg)
			}
		}
		crateArgs := crateArgNames(crateCall)
		for i, arg := range call.Args {
			if i == crateIdx {
				continue
			}
			// The crate's value wins, or was merged into a list above.
			if attr, ok := attrName(arg); ok && (crateAttrs[attr] || crateListAttrs[attr]) && crateArgs[attr] {
				continue
			}
			newArgs = append(newArgs, arg)
		}
		// GN's `rustc_macro()` adds this config to its own unit tests, because
		// rustc only links the `proc_macro` crate into proc-macro crates.
		if cfn, ok := crateCall.Fn.(*syntax.Ident); ok && procMacroRules[cfn.Name] {
			newArgs = append(newArgs, attrExpr("build_flags", &syntax.ListExpr{
				List: []syntax.Expr{stringLiteral("//build/config/rust:proc_macro_test")},
			}))
		}
		call.Args = newArgs
	}
	return nil
}

// ffxPluginExecCrate returns a synthetic call representing the `<name>`
// execution crate defined by `ffx_plugin(name = "<name>")`, which implicitly
// depends on `:<name>_args`, or nil if the plugin has no `srcs` (pure
// subcommand suite).
func ffxPluginExecCrate(call *syntax.CallExpr, name string) *syntax.CallExpr {
	argsDep := stringLiteral(":" + name + "_args")
	var args []syntax.Expr
	hasSrcs := false
	hasDeps := false
	for _, arg := range call.Args {
		if skip, err := hasSkipAnnotation(arg); err != nil || skip {
			continue
		}
		attr, ok := attrName(arg)
		if !ok {
			continue
		}
		kv := arg.(*syntax.BinaryExpr)
		switch attr {
		case "srcs":
			if !isEmptyListExpr(kv.Y) {
				hasSrcs = true
			}
		case "deps":
			hasDeps = true
			copied := *kv
			copied.Y = appendToListExpr(copied.Y, argsDep)
			args = append(args, &copied)
			continue
		}
		args = append(args, arg)
	}
	if !hasSrcs {
		return nil
	}
	if !hasDeps {
		args = append(args, attrExpr("deps", &syntax.ListExpr{List: []syntax.Expr{argsDep}}))
	}
	return &syntax.CallExpr{Fn: call.Fn, Args: args}
}

// ffxPluginArgsCrate returns a synthetic call representing the `<name>_args`
// crate defined by `ffx_plugin(name = "<name>")`.
func ffxPluginArgsCrate(call *syntax.CallExpr, name string) *syntax.CallExpr {
	var args []syntax.Expr
	var depsArg *syntax.BinaryExpr
	hasCrateRoot := false
	hasPluginDeps := false
	for _, arg := range call.Args {
		if skip, err := hasSkipAnnotation(arg); err != nil || skip {
			continue
		}
		attr, ok := attrName(arg)
		if !ok {
			continue
		}
		kv := arg.(*syntax.BinaryExpr)
		switch attr {
		case "args_srcs":
			args = append(args, renameAttrExpr(kv, "srcs"))
		case "args_crate_root":
			hasCrateRoot = true
			args = append(args, renameAttrExpr(kv, "crate_root"))
		case "edition":
			args = append(args, arg)
		case "args_deps":
			depsArg = renameAttrExpr(kv, "deps")
		case "plugin_deps":
			if !isEmptyListExpr(kv.Y) {
				hasPluginDeps = true
			}
		}
	}
	if !hasCrateRoot {
		args = append(args, attrExpr("crate_root", stringLiteral("src/args.rs")))
	}
	if hasPluginDeps {
		subCmdDep := stringLiteral(":" + name + "_sub_command")
		if depsArg != nil {
			depsArg.Y = appendToListExpr(depsArg.Y, subCmdDep)
		} else {
			depsArg = attrExpr("deps", &syntax.ListExpr{List: []syntax.Expr{subCmdDep}})
		}
	}
	if depsArg != nil {
		args = append(args, depsArg)
	}
	return &syntax.CallExpr{Fn: call.Fn, Args: args}
}

func renameAttrExpr(kv *syntax.BinaryExpr, name string) *syntax.BinaryExpr {
	copied := *kv
	copied.X = &syntax.Ident{Name: name}
	return &copied
}

func isEmptyListExpr(expr syntax.Expr) bool {
	list, ok := unwrapParenExpr(expr).(*syntax.ListExpr)
	return ok && len(list.List) == 0
}

func attrExpr(name string, val syntax.Expr) *syntax.BinaryExpr {
	return &syntax.BinaryExpr{
		X:  &syntax.Ident{Name: name},
		Op: syntax.EQ,
		Y:  val,
	}
}

func stringLiteral(val string) *syntax.Literal {
	return &syntax.Literal{
		Token: syntax.STRING,
		Raw:   fmt.Sprintf("%q", val),
		Value: val,
	}
}

func appendToListExpr(expr syntax.Expr, elem syntax.Expr) syntax.Expr {
	if list, ok := unwrapParenExpr(expr).(*syntax.ListExpr); ok {
		return &syntax.ListExpr{List: append(slices.Clone(list.List), elem)}
	}
	return &syntax.BinaryExpr{
		X:  expr,
		Op: syntax.PLUS,
		Y:  &syntax.ListExpr{List: []syntax.Expr{elem}},
	}
}

// crateArgNames returns the names of the attributes set on a call, except
// those marked with `# @bazel2gn:skip`.
func crateArgNames(call *syntax.CallExpr) map[string]bool {
	names := map[string]bool{}
	for _, arg := range call.Args {
		if skip, err := hasSkipAnnotation(arg); err != nil || skip {
			continue
		}
		if attr, ok := attrName(arg); ok {
			names[attr] = true
		}
	}
	return names
}

// callOfStmt returns the call expression of a top-level target definition.
func callOfStmt(stmt syntax.Stmt) (*syntax.CallExpr, bool) {
	exprStmt, ok := stmt.(*syntax.ExprStmt)
	if !ok {
		return nil, false
	}
	call, ok := exprStmt.X.(*syntax.CallExpr)
	return call, ok
}

// attrName returns the attribute name of a `name = value` call argument.
func attrName(arg syntax.Expr) (string, bool) {
	binaryExpr, ok := arg.(*syntax.BinaryExpr)
	if !ok || binaryExpr.Op != syntax.EQ {
		return "", false
	}
	ident, ok := binaryExpr.X.(*syntax.Ident)
	if !ok {
		return "", false
	}
	return ident.Name, true
}

// callStringAttr returns the value of a string literal attribute of a call.
func callStringAttr(call *syntax.CallExpr, name string) (string, bool) {
	for _, arg := range call.Args {
		if attr, ok := attrName(arg); ok && attr == name {
			val, err := stringLiteralValue(arg.(*syntax.BinaryExpr).Y)
			return val, err == nil
		}
	}
	return "", false
}

// stringLiteralValue returns the value of a Starlark string literal.
func stringLiteralValue(expr syntax.Expr) (string, error) {
	lit, ok := unwrapParenExpr(expr).(*syntax.Literal)
	if !ok {
		return "", fmt.Errorf("expected a string literal, got %T", expr)
	}
	val, ok := lit.Value.(string)
	if !ok {
		return "", fmt.Errorf("expected a string literal, got %s", strings.TrimSpace(lit.Raw))
	}
	return val, nil
}
