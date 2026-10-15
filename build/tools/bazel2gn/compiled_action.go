// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn

import (
	"fmt"
	"regexp"
	"strings"

	"go.starlark.net/syntax"
)

// compiledActionRule is the name of the Bazel rule defined in
// //build/bazel/rules/compiled_action.bzl, which is converted to the GN
// `compiled_action()` template defined in //build/compiled_action.gni.
const compiledActionRule = "compiled_action"

// compiledActionRuleDir is the Bazel `$(RULEDIR)` variable supported in
// `compiled_action()` args.
const compiledActionRuleDir = "$(RULEDIR)"

// compiledActionGNRuleDir is the GN path of the directory containing the
// outputs of a `compiled_action()`, i.e. the GN equivalent of `$(RULEDIR)`.
const compiledActionGNRuleDir = "$target_gen_dir"

// compiledActionLocationRE matches the `$(location <label>)` and
// `$(execpath <label>)` substitutions supported in `compiled_action()` args.
var compiledActionLocationRE = regexp.MustCompile(`\$\((location|execpath) ([^()\s]+)\)`)

// compiledActionOutToGN converts a single Bazel `compiled_action()` output
// (an element of `outs`) to its GN path.
//
// Bazel outputs are relative to the package's output directory, the GN
// equivalent of which is `$target_gen_dir`. A path overwrite annotation, if
// any, takes precedence.
func compiledActionOutToGN(lit *syntax.Literal) (string, error) {
	if path, ok := overwrittenPath(lit); ok {
		return path, nil
	}
	out, ok := lit.Value.(string)
	if !ok {
		return "", fmt.Errorf("expected string literal in compiled_action outs, got %s", lit.Raw)
	}
	return compiledActionGNRuleDir + "/" + out, nil
}

// compiledActionOutsTransformer is a transformer converting Bazel
// `compiled_action()` outputs to GN paths, see `compiledActionOutToGN`.
func compiledActionOutsTransformer(expr syntax.Expr) (syntax.Expr, error) {
	lit, ok := expr.(*syntax.Literal)
	if !ok {
		return expr, nil
	}
	path, err := compiledActionOutToGN(lit)
	if err != nil {
		return nil, err
	}
	lit.Raw = gnQuote(path)
	return lit, nil
}

// compiledActionOuts returns a map from the Bazel labels that can refer to
// the outputs of a `compiled_action()` call (e.g. "foo.txt" and ":foo.txt")
// to their GN paths.
func compiledActionOuts(call *syntax.CallExpr) (map[string]string, error) {
	outs := map[string]string{}
	for _, arg := range call.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			continue
		}
		ident, ok := binaryExpr.X.(*syntax.Ident)
		if !ok || ident.Name != "outs" {
			continue
		}
		if _, ok := overwrittenRaw(binaryExpr); ok {
			// The GN outputs are set by hand, so they can't be referred to in args.
			return outs, nil
		}
		list, ok := unwrapParenExpr(binaryExpr.Y).(*syntax.ListExpr)
		if !ok {
			return nil, fmt.Errorf("compiled_action outs must be a list literal, got %T", binaryExpr.Y)
		}
		for _, elm := range list.List {
			lit, ok := elm.(*syntax.Literal)
			if !ok {
				return nil, fmt.Errorf("compiled_action outs must only contain string literals, got %T", elm)
			}
			path, err := compiledActionOutToGN(lit)
			if err != nil {
				return nil, err
			}
			out := lit.Value.(string)
			outs[out] = path
			outs[":"+out] = path
		}
	}
	return outs, nil
}

// compiledActionArgsTransformer returns a transformer that converts the `args`
// attribute of a Bazel `compiled_action()` to GN.
//
// `outs` maps from Bazel labels of the outputs of the action to their GN
// paths, see `compiledActionOuts`.
//
// Each element in args must be a string literal, and supports the following
// substitutions:
//
//   - `$(location <label>)` and `$(execpath <label>)`, where `<label>` is in
//     `outs` or is a source file, are converted to
//     `rebase_path("<path>", root_build_dir)`.
//   - `$(RULEDIR)` is converted to
//     `rebase_path(target_gen_dir, root_build_dir)`.
//
// Any other `$(...)` substitution is an error.
func compiledActionArgsTransformer(outs map[string]string) transformer {
	return func(expr syntax.Expr) (syntax.Expr, error) {
		if _, ok := unwrapParenExpr(expr).(*syntax.ListExpr); ok {
			return expr, nil
		}
		lit, ok := expr.(*syntax.Literal)
		if !ok {
			return nil, fmt.Errorf("compiled_action args must be a list of string literals, got %T", expr)
		}
		if raw, ok := overwrittenRaw(lit); ok {
			lit.Raw = raw
			return lit, nil
		}
		arg, ok := lit.Value.(string)
		if !ok {
			return nil, fmt.Errorf("compiled_action args must only contain string literals, got %s", lit.Raw)
		}
		gnArg, err := compiledActionArgToGN(arg, outs)
		if err != nil {
			return nil, err
		}
		lit.Raw = gnArg
		return lit, nil
	}
}

// compiledActionArgToGN converts a single `compiled_action()` argument to a
// GN expression. See `compiledActionArgsTransformer` for supported substitutions.
func compiledActionArgToGN(arg string, outs map[string]string) (string, error) {
	var parts []string
	for len(arg) > 0 {
		start := strings.Index(arg, "$(")
		if start == -1 {
			parts = append(parts, gnQuoteLiteral(arg))
			break
		}
		if start > 0 {
			parts = append(parts, gnQuoteLiteral(arg[:start]))
			arg = arg[start:]
		}

		if strings.HasPrefix(arg, compiledActionRuleDir) {
			parts = append(parts, "rebase_path(target_gen_dir, root_build_dir)")
			arg = arg[len(compiledActionRuleDir):]
			continue
		}

		m := compiledActionLocationRE.FindStringSubmatchIndex(arg)
		if m == nil || m[0] != 0 {
			end := strings.Index(arg, ")")
			if end == -1 {
				end = len(arg) - 1
			}
			return "", fmt.Errorf("unsupported substitution %q in compiled_action args, only $(location <label>), $(execpath <label>), and %s are supported", arg[:end+1], compiledActionRuleDir)
		}
		label := arg[m[4]:m[5]]
		path, ok := outs[label]
		if !ok {
			var err error
			path, err = bazelFileLabelToGNPath(label)
			if err != nil {
				return "", err
			}
		}
		parts = append(parts, fmt.Sprintf("rebase_path(%s, root_build_dir)", gnQuote(path)))
		arg = arg[m[1]:]
	}
	if len(parts) == 0 {
		return `""`, nil
	}
	return strings.Join(parts, " + "), nil
}

// bazelFileLabelToGNPath converts a Bazel label of a source file to a GN path.
//
// For example:
//
//	foo.txt             -> foo.txt
//	:foo.txt            -> foo.txt
//	//path/to:foo.txt   -> //path/to/foo.txt
//	//:prebuilt/foo.txt -> //prebuilt/foo.txt
func bazelFileLabelToGNPath(label string) (string, error) {
	if strings.HasPrefix(label, "@") {
		return "", fmt.Errorf("labels in external repositories are not supported, got %q", label)
	}
	if strings.HasPrefix(label, ":") {
		return label[1:], nil
	}
	if !strings.HasPrefix(label, "//") {
		return label, nil
	}
	pkg, file, ok := strings.Cut(label[2:], ":")
	if !ok {
		return "", fmt.Errorf("expected a label of a file in the form //path/to:file, got %q", label)
	}
	if pkg == "" {
		return "//" + file, nil
	}
	return "//" + pkg + "/" + file, nil
}

// gnQuote returns s as a quoted GN string literal. `$` is kept as-is so GN
// variables in paths (e.g. `$target_gen_dir`) are expanded.
func gnQuote(s string) string {
	s = strings.ReplaceAll(s, `\`, `\\`)
	s = strings.ReplaceAll(s, `"`, `\"`)
	return `"` + s + `"`
}

// gnQuoteLiteral returns s as a quoted GN string literal that GN does not
// expand, i.e. `$` is escaped too.
func gnQuoteLiteral(s string) string {
	quoted := gnQuote(s)
	return strings.ReplaceAll(quoted, "$", `\$`)
}
