// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn

import (
	"errors"
	"fmt"
	"strings"

	"go.starlark.net/syntax"
)

// rustcCheckCfgBuildFlagsToGN converts a Bazel `rustc_check_cfg_build_flags()`
// macro call to a GN `config()` target.
func rustcCheckCfgBuildFlagsToGN(call *syntax.CallExpr) ([]string, error) {
	var name string
	var allCfgs []string
	var visArg *syntax.BinaryExpr

	for _, arg := range call.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			return nil, fmt.Errorf("rustc_check_cfg_build_flags only accepts keyword arguments, got %T", arg)
		}

		ident, ok := binaryExpr.X.(*syntax.Ident)
		if !ok {
			return nil, fmt.Errorf("expected identifier for argument name, got %T", binaryExpr.X)
		}

		switch ident.Name {
		case "name":
			lines, err := exprToGN(binaryExpr.Y, nil)
			if err != nil {
				return nil, fmt.Errorf("converting target name: %v", err)
			}
			name = strings.Join(lines, "\n")
		case "cfgs":
			listExpr, ok := unwrapParenExpr(binaryExpr.Y).(*syntax.ListExpr)
			if !ok {
				return nil, fmt.Errorf("expected list literal for cfgs argument, got %T", binaryExpr.Y)
			}
			for _, elm := range listExpr.List {
				lit, ok := elm.(*syntax.Literal)
				if !ok {
					return nil, fmt.Errorf("expected string literal in cfgs list, got %T", elm)
				}
				val, ok := lit.Value.(string)
				if !ok {
					return nil, fmt.Errorf("expected string literal in cfgs list, got %T", lit.Value)
				}
				allCfgs = append(allCfgs, val)
			}
		case "features":
			listExpr, ok := unwrapParenExpr(binaryExpr.Y).(*syntax.ListExpr)
			if !ok {
				return nil, fmt.Errorf("expected list literal for features argument, got %T", binaryExpr.Y)
			}
			var quotedFeatures []string
			for _, elm := range listExpr.List {
				lit, ok := elm.(*syntax.Literal)
				if !ok {
					return nil, fmt.Errorf("expected string literal in features list, got %T", elm)
				}
				val, ok := lit.Value.(string)
				if !ok {
					return nil, fmt.Errorf("expected string literal in features list, got %T", lit.Value)
				}
				quotedFeatures = append(quotedFeatures, fmt.Sprintf(`\"%s\"`, val))
			}
			if len(quotedFeatures) > 0 {
				allCfgs = append(allCfgs, fmt.Sprintf(`feature, values(%s)`, strings.Join(quotedFeatures, ", ")))
			}
		case "visibility":
			visArg = binaryExpr
		default:
			return nil, fmt.Errorf("unexpected argument %q in rustc_check_cfg_build_flags", ident.Name)
		}
	}

	if name == "" {
		return nil, errors.New("missing `name` attribute in rustc_check_cfg_build_flags")
	}

	var flags []syntax.Expr
	for _, c := range allCfgs {
		flags = append(flags, &syntax.Literal{
			Raw: fmt.Sprintf(`"--check-cfg=cfg(%s)"`, c),
		})
	}

	ret := []string{fmt.Sprintf("config(%s) {", name)}

	if len(flags) > 0 {
		listLines, err := listExprToGN(&syntax.ListExpr{List: flags}, nil)
		if err != nil {
			return nil, fmt.Errorf("converting rustflags list to GN: %v", err)
		}
		if len(listLines) > 0 {
			listLines[0] = "rustflags = " + listLines[0]
		}
		ret = append(ret, indent(listLines, 1)...)
	}

	if visArg != nil {
		lines, err := attrAssignmentToGN(visArg, "config", nil /*compiledActionOutputs*/, false /*appendAspectHints*/)
		if err != nil {
			return nil, fmt.Errorf("converting visibility attribute to GN: %v", err)
		}
		ret = append(ret, indent(lines, 1)...)
	}

	ret = append(ret, "}")
	return ret, nil
}
