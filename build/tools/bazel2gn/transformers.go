// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn

import (
	"fmt"
	"strings"

	"go.starlark.net/syntax"
)

// transformer is a function type that can be used by `exprToGN` to apply
// special transformations to expression nodes before conversion.
//
// This is useful for rewriting certain string values, recording values during
// traversal, or even restructuring the syntax tree.
type transformer func(syntax.Expr) (syntax.Expr, error)

// bazelVisibilityToGN converts Bazel visibility values [0] to GN [1].
//
// NOTE: Bazel visibility is based on package groups [2], while GN visibility is
// based on target. However it should be possible to create matching groups in
// GN for the exact same visibility control in the most granular cases.
//
// [0] https://bazel.build/concepts/visibility#visibility-specifications
// [1] https://gn.googlesource.com/gn/+/main/docs/reference.md#var_visibility
// [2] https://bazel.build/reference/be/functions#package_group
func bazelVisibilityToGN(expr syntax.Expr) (syntax.Expr, error) {
	lit, ok := expr.(*syntax.Literal)
	if !ok {
		return expr, nil
	}
	switch {
	case lit.Raw == `"//visibility:public"`:
		lit.Raw = `"*"`
	case lit.Raw == `"//visibility:private"`:
		lit.Raw = `":*"`
	case strings.HasSuffix(lit.Raw, `:__pkg__"`):
		lit.Raw = strings.ReplaceAll(lit.Raw, `:__pkg__"`, `:*"`)
	case strings.HasSuffix(lit.Raw, `:__subpackages__"`):
		lit.Raw = strings.ReplaceAll(lit.Raw, `:__subpackages__"`, `/*"`)
	default:
		// This is a Bazel visibility on a package group, it should stay unchanged.
		// In GN there should be a target matching the path of this package group.
	}
	return lit, nil
}

// bazelCOptToGNConfig converts Bazel copts values to GN configs.
func bazelCOptToGNConfig(expr syntax.Expr) (syntax.Expr, error) {
	lit, ok := expr.(*syntax.Literal)
	if !ok {
		return expr, nil
	}
	coptWithoutQuotes := lit.Raw[1 : len(lit.Raw)-1]
	config, ok := coptToConfig[coptWithoutQuotes]
	if !ok {
		return nil, fmt.Errorf("unexpected copt %s", coptWithoutQuotes)
	}
	lit.Raw = fmt.Sprintf(`"%s"`, config)
	return lit, nil
}

// bazelDepToGN converts Bazel dependency paths to GN ones.
//
// This function is expected to encapsulate information specific to the Fuchsia
// build system. Ideally the problem solved here should be solved in the build
// system (e.g. move location of build files), so this tool packs less surprises.
func bazelDepToGN(expr syntax.Expr) (syntax.Expr, error) {
	lit, ok := expr.(*syntax.Literal)
	if !ok {
		return expr, nil
	}
	lit.Raw = thirdPartyRustCrateRE.ReplaceAllString(
		lit.Raw,
		`"//third_party/rust_crates:`,
	)
	return lit, nil
}
