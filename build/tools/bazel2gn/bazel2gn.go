// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package bazel2gn

import (
	"errors"
	"fmt"
	"strings"

	"go.starlark.net/syntax"
)

// Parse takes a path to a starlark file and returns a parsed AST.
//
// This delegates to the starlark parser library we are using.
// Create a function wrapper here to capture the settings and modes we use during parsing.
func Parse(path string) (*syntax.File, error) {
	opts := syntax.FileOptions{
		// Empty means default file-level settings for parsing.
	}
	return opts.Parse(path, nil, syntax.RetainComments)
}

// indent indents input lines by input levels.
func indent(lines []string, level int) []string {
	var indented []string
	prefix := strings.Repeat(indentPrefix, level)
	for _, l := range lines {
		indented = append(indented, prefix+l)
	}
	return indented
}

// StmtToGN converts a Bazel statement [0] to GN.
//
// [0] https://github.com/bazelbuild/starlark/blob/master/spec.md#statements
func StmtToGN(stmt syntax.Stmt) ([]string, error) {
	switch v := stmt.(type) {
	case *syntax.LoadStmt:
		// Load statements are ignored during conversion.
		return nil, nil
	case *syntax.ExprStmt:
		return exprToGN(v.X, nil)
	case *syntax.AssignStmt:
		return assignStmtToGN(v)
	default:
		return nil, fmt.Errorf("statement of type %T is not supported to be converted to GN, node details: %#v", stmt, stmt)
	}
}

// exprToGN converts a Bazel expression [0] to GN.
//
// It applies input transformers first before delegating to more specific
// conversion functions based on expression type.
//
// [0] https://github.com/bazelbuild/starlark/blob/master/spec.md#expressions
func exprToGN(expr syntax.Expr, transformers []transformer) ([]string, error) {
	for _, ts := range transformers {
		var err error
		expr, err = ts(expr)
		if err != nil {
			return nil, fmt.Errorf("applying special handler before converting expression: %v", err)
		}
	}

	switch v := expr.(type) {
	case *syntax.CallExpr:
		// NOTE: I'm not sure whether we need to plumb transformers here, so far it
		// is not necessary. callExprToGN should be a top-level entry point for
		// macro and rules.
		return callExprToGN(v)
	case *syntax.BinaryExpr:
		return binaryExprToGN(v, transformers)
	case *syntax.Ident:
		return identToGN(v)
	case *syntax.Literal:
		return []string{v.Raw}, nil
	case *syntax.ListExpr:
		return listExprToGN(v, transformers)
	case *syntax.DictExpr:
		return dictExprToGN(v, transformers)
	default:
		return nil, fmt.Errorf("expression of type %T is not supported when converting to GN, node details: %#v", expr, expr)
	}
}

// identToGN converts a Bazel identifier to GN.
func identToGN(ident *syntax.Ident) ([]string, error) {
	val, ok := specialIdentifiers[ident.Name]
	if !ok {
		val = ident.Name
	}
	return []string{val}, nil
}

// assignStmtToGN converts a Bazel assignment statement [0] to GN.
//
// [0] https://github.com/bazelbuild/starlark/blob/master/spec.md#assignments
func assignStmtToGN(stmt *syntax.AssignStmt) ([]string, error) {
	lhs, err := exprToGN(stmt.LHS, nil)
	if err != nil {
		return nil, fmt.Errorf("converting lhs of assignment statement: %v", err)
	}
	if len(lhs) == 0 {
		return nil, errors.New("lhs of assignment statement is unexpectedly empty")
	}

	rhs, err := exprToGN(stmt.RHS, nil)
	if err != nil {
		return nil, fmt.Errorf("converting rhs of assignment statement: %v", err)
	}
	if len(rhs) == 0 {
		return nil, errors.New("rhs of assignment statement is unexpectedly empty")
	}

	ret := []string{fmt.Sprintf("%s %s %s", lhs[0], opToGN(stmt.Op), rhs[0])}
	ret = append(ret, rhs[1:]...)
	return ret, nil
}

// targetCompatibleWithToGNConditions converts a Bazel `target_compatible_with`
// expression to GN conditions.
//
// It supports identifiers representing a list of constraints and lists of
// constraints containing literals and identifiers.
// It does not support concatenating multiple lists of constraints.
func targetCompatibleWithToGNConditions(expr syntax.Expr) ([]string, error) {
	switch v := expr.(type) {
	case *syntax.Ident:
		// An identifier not in a list. It must be a variable representing a list.
		gnCondition, ok := bazelConstraintListVarsToGNConditions[v.Name]
		if !ok {
			return nil, fmt.Errorf("unsupported target_compatible_with variable: %v", v.Name)
		}
		return []string{gnCondition}, nil
	case *syntax.ListExpr:
		var gnConditions []string
		for _, elm := range v.List {
			switch elm := elm.(type) {
			case *syntax.Literal:
				val, ok := elm.Value.(string)
				if !ok {
					return nil, fmt.Errorf("unexpected literal type in target_compatible_with: %T", elm.Value)
				}
				gnCondition, ok := bazelConstraintsToGNConditions[val]
				if !ok {
					return nil, fmt.Errorf("unsupported target_compatible_with variable: %v", val)
				}
				gnConditions = append(gnConditions, gnCondition)
			case *syntax.Ident:
				gnCondition, ok := bazelConstraintsToGNConditions[elm.Name]
				if !ok {
					return nil, fmt.Errorf("unsupported target_compatible_with variable: %v", elm.Name)
				}
				gnConditions = append(gnConditions, gnCondition)
			default:
				return nil, fmt.Errorf("unsupported expression type in target_compatible_with list: %T", elm)
			}
		}
		return gnConditions, nil
	default:
		return nil, fmt.Errorf("unsupported type %T as value to target_compatible_with in Bazel, node details: %#v", expr, expr)
	}
}

// callExprToGN converts a Bazel call expression [0] to GN. These calls should
// be macro or Bazel rules known to the converter.
//
// [0] https://github.com/bazelbuild/starlark/blob/master/spec.md#function-and-method-calls
func callExprToGN(expr *syntax.CallExpr) ([]string, error) {
	fn := expr.Fn.(*syntax.Ident)
	bazelRule := fn.Name
	gnTemplateName, ok := bazelRuleToGNTemplate[bazelRule]
	if !ok {
		return nil, fmt.Errorf("%s is not a known Bazel rule to convert to GN", bazelRule)
	}

	if gnTemplateName == "__NO_GN_EQUIVALENT__" {
		return nil, nil
	}

	// TODO(jayzhuang): Handle package level settings, e.g. visibility.
	if bazelRule == "package" {
		return nil, nil
	}

	attrsToOmit := attrsToOmitByRules[bazelRule]

	// Loops through all arguments to handle special ones first.
	var name string
	var remainingArgs []*syntax.BinaryExpr
	var wrappingConditions []string
	for _, arg := range expr.Args {
		binaryExpr, ok := arg.(*syntax.BinaryExpr)
		if !ok || binaryExpr.Op != syntax.EQ {
			return nil, fmt.Errorf("only attribute assignments (e.g. `attr = value`) are allowed in Bazel targets to be converted to GN, got %#v", arg)
		}
		ident, ok := binaryExpr.X.(*syntax.Ident)
		if !ok {
			return nil, fmt.Errorf("unexpected node type on the left hand side of binary expression in target definition, want syntax.Ident, got %T", binaryExpr.X)
		}
		if attrsToOmit[ident.Name] {
			continue
		}
		if ident.Name == "name" {
			lines, err := exprToGN(binaryExpr.Y, nil)
			if err != nil {
				return nil, fmt.Errorf("converting target name: %v", err)
			}
			name = strings.Join(lines, "\n")

			// Handle differences in naming conventions.
			if bazelRule == "idk_host_tool" || bazelRule == "idk_cc_binary_host_tool" {
				// In GN, the template did not automatically append "_sdk" to
				// the name of the atom target and it was included in the name
				// passed to the template. In Bazel, the macro is consistent
				// with other IDK atom macros. Handle this by appending "_sdk".
				if !(len(name) > 1 && name[len(name)-1] == '"') {
					return nil, fmt.Errorf("expected a quoted string for name, but got %s", name)
				}
				name = name[:len(name)-1] + "_sdk\""
			}

			continue
		}
		if ident.Name == "target_compatible_with" {
			var err error
			wrappingConditions, err = targetCompatibleWithToGNConditions(binaryExpr.Y)
			if err != nil {
				return nil, fmt.Errorf("converting Bazel target_compatible_with to GN conditions: %v", err)
			}
			continue
		}
		remainingArgs = append(remainingArgs, binaryExpr)
	}
	if name == "" {
		return nil, errors.New("missing `name` attribute in Bazel target")
	}

	ret := []string{fmt.Sprintf("%s(%s) {", gnTemplateName, name)}

	// Loop through all args again to actually build the content of this target.
	for _, arg := range remainingArgs {
		lines, err := attrAssignmentToGN(arg, bazelRule)
		if err != nil {
			return nil, fmt.Errorf("converting Bazel attribute to GN: %v", err)
		}
		ret = append(ret, indent(lines, 1)...)
	}

	if extra, ok := extraGnExpressionByRules[bazelRule]; ok {
		ret = append(ret, indent([]string{extra}, 1)...)
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

// convertAttrName converts input Bazel attrName to the corresponding parameter name in GN.
//
// This function takes the current Bazel rule being converted into consideration.
func convertAttrName(attrName, bazelRule string) string {
	ret, ok := commonAttrMap[attrName]
	if ok {
		return ret
	}
	ruleAttrMap, ok := attrMapsByRules[bazelRule]
	if !ok {
		return attrName
	}
	ret, ok = ruleAttrMap[attrName]
	if ok {
		return ret
	}
	return attrName
}

// hasClearAnnotation returns true if the input expr has a clear annotation comment.
//
// This function only picks up suffix comments, for example:
//
// ```
//
//	# This comment is NOT considered
//	copts = [] # This comment is considered
//
//	copts = [ # This comment is NOT considered
//	] # This comment is considered
//
// ```
func hasClearAnnotation(expr syntax.Expr) bool {
	comments := expr.Comments()
	if comments != nil {
		for _, c := range comments.Suffix {
			if c.Text == clearAnnotation {
				return true
			}
		}
	}
	return false
}

// attrAssignmentToGN converts a Bazel assignment [0] to GN. These assignments
// are used to assign values to fields during target definitions in Bazel.
//
// This function handles the special cases in attribute assignments, e.g. when
// select calls are involved. This is done through applying node transformers
// funcs during the traversal.
//
// NOTE: Assignment is a special binary expression with operator =.
//
// [0] https://github.com/bazelbuild/starlark/blob/master/spec.md#assignments
func attrAssignmentToGN(expr *syntax.BinaryExpr, bazelRule string) ([]string, error) {
	lhs, ok := expr.X.(*syntax.Ident)
	if !ok {
		return nil, fmt.Errorf("expecting an identifier on the left hand side of attribute assignment, got %T", expr.X)
	}
	attrName := convertAttrName(lhs.Name, bazelRule)

	// Intercept genrule cmd assignment and convert it directly.
	//
	// NOTE: This means bazel2gn does NOT support select calls in `cmd` currently.
	if bazelRule == "genrule" && attrName == "cmd" {
		return genruleCmdToGN(expr.Y)
	}

	op, ok := attrGNAssignmentOps[attrName]
	if !ok {
		op = "="
	}

	if lhs.Name == "fuchsia_deps" {
		op = "+="
	}

	// By default "configs" uses += to concatenate values set in Bazel with =.
	// Check for clear annotation to explicitly clear configs in GN, which
	// requires using =.
	//
	// TODO(https://fxbug.dev/430953918): Figure out a better way to handle configs and public_configs conversion.
	if attrName == "configs" && hasClearAnnotation(expr) {
		op = "="
	}

	var transformers []transformer
	switch attrName {
	case "visibility":
		transformers = append(transformers, bazelVisibilityToGN)
	case "deps", "public_deps", "test_deps", "proc_macro_deps":
		transformers = append(transformers, bazelDepToGN)
	case "configs":
		transformers = append(transformers, bazelCOptToGNConfig)
	case "sources", "outputs":
		transformers = append(transformers, bazelPathsToGN)
	}

	// This is a simple `attr = select(...)`, convert in-place.
	if isSelectCall(expr.Y) {
		return selectToGN(attrName, op, expr.Y.(*syntax.CallExpr), transformers)
	}

	// It is not a simple `select` call on the RHS, and `select`s are found in
	// subtree, so assume this is list concatenation with `select`s in them.
	//
	// NOTE: Currently `select`s are only supported in list concatenation when
	// they are used in binary expressions. Other usages will fail this call.
	if hasSelectCall(expr.Y) {
		lc, err := listConcatWithSelectToGN(attrName, expr.Y, transformers)
		if err != nil {
			return nil, err
		}
		// Start with an empty list so it's easy to += new elements from later
		// conversions.
		return append([]string{fmt.Sprintf("%s %s []", attrName, op)}, lc...), nil
	}

	rhs, err := exprToGN(expr.Y, transformers)
	if err != nil {
		return nil, fmt.Errorf("converting rhs of binary expression: %v", err)
	}
	if len(rhs) == 0 {
		return nil, errors.New("rhs hand side of binary expression is unexpectedly empty")
	}

	ret := []string{fmt.Sprintf("%s %s %s", attrName, op, rhs[0])}
	ret = append(ret, rhs[1:]...)

	if lhs.Name == "fuchsia_deps" {
		ret = append([]string{"if (is_fuchsia) {"}, indent(ret, 1)...)
		ret = append(ret, "}")
	}

	// Handle any additional work necessary for specific assignments.
	if attrName == "sdk_headers_for_internal_use" {
		// While the files in GN's `sdk_headers_for_internal_use` are included
		// in `public` (or `sources`), that is not the cases for Bazel's
		// `hdrs_for_internal_use`. To match the GN behavior, add all the files
		// specified in `hdrs_for_internal_use` to `public` in the GN target.
		// For more information, see `idk_cc_source_library()`.`
		ret = append(ret, "public += sdk_headers_for_internal_use")
	}

	return ret, nil
}

// binaryExprToGN converts a general Bazel binary expression [0] to GN.
//
// [0] https://github.com/bazelbuild/starlark/blob/master/spec.md#binary-operators
func binaryExprToGN(expr *syntax.BinaryExpr, transformers []transformer) ([]string, error) {
	lhs, err := exprToGN(expr.X, transformers)
	if err != nil {
		return nil, fmt.Errorf("converting lhs of binary expression: %v", err)
	}
	if len(lhs) == 0 {
		return nil, errors.New("lhs of binary expression is unexpectedly empty")
	}

	rhs, err := exprToGN(expr.Y, transformers)
	if err != nil {
		return nil, fmt.Errorf("converting rhs of binary expression: %v", err)
	}
	if len(rhs) == 0 {
		return nil, errors.New("rhs hand side of binary expression is unexpectedly empty")
	}

	ret := lhs[:len(lhs)-1]
	ret = append(ret, fmt.Sprintf("%s %s %s", lhs[len(lhs)-1], opToGN(expr.Op), rhs[0]))
	ret = append(ret, rhs[1:]...)
	return ret, nil
}

func opToGN(op syntax.Token) string {
	ret, ok := specialTokens[op]
	if !ok {
		return op.String()
	}
	return ret
}

// listExprToGN converts a Bazel list expression [0] to GN.
//
// [0] https://github.com/bazelbuild/starlark/blob/master/spec.md#lists
func listExprToGN(expr *syntax.ListExpr, transformers []transformer) ([]string, error) {
	ret := []string{"["}

	for _, elm := range expr.List {
		elmLines, err := exprToGN(elm, transformers)
		if err != nil {
			return nil, fmt.Errorf("converting list element: %v", err)
		}
		if len(elmLines) == 0 {
			continue
		}
		elmLines[len(elmLines)-1] = elmLines[len(elmLines)-1] + ","
		ret = append(ret, indent(elmLines, 1)...)
	}

	ret = append(ret, "]")
	return ret, nil
}

// dictExprToGN converts a Bazel dictionary expression to a GN scope.
func dictExprToGN(expr *syntax.DictExpr, transformers []transformer) ([]string, error) {
	ret := []string{"{"}

	for _, entry := range expr.List {
		entryDictEntry, ok := entry.(*syntax.DictEntry)
		if !ok {
			return nil, fmt.Errorf("unexpected node type in dictionary entry: %T", entry)
		}

		key, err := exprToGN(entryDictEntry.Key, transformers)
		if err != nil {
			return nil, fmt.Errorf("converting dictionary key: %v", err)
		}

		value, err := exprToGN(entryDictEntry.Value, transformers)
		if err != nil {
			return nil, fmt.Errorf("converting dictionary value: %v", err)
		}

		// In GN, keys are identifiers, so they should not be quoted.
		// Starlark dictionary keys are strings, so they are quoted.
		// We need to remove the quotes from the key.
		key[0] = strings.Trim(key[0], `"`)

		lines := []string{fmt.Sprintf("%s = %s", key[0], value[0])}
		if len(value) > 1 {
			lines = append(lines, value[1:]...)
		}
		ret = append(ret, indent(lines, 1)...)
	}

	ret = append(ret, "}")
	return ret, nil
}

// HasSkipAnnotation returns true if the given statement has a skip annotation.
//
// A skip annotation is a comment that exactly matches `@bazel2gn:skip`, and
// right above the statement. The statement right after this annotation will be
// skipped during conversion.
//
// For example this is a skip annotation:
//
// ```
//
//	# Some other comment
//	# @bazel2gn:skip
//	go_library(
//	  ...
//	)
//
// ```
//
// And these are NOT skip annotations:
//
// ```
//
//	# @bazel2gn:skip
//	# Skip annotation should be right above the statement.
//	go_library(
//		# @bazel2gn:skip
//		...
//	) # @bazel2gn:skip
//
// ```
func HasSkipAnnotation(stmt syntax.Stmt) (bool, error) {
	comments := stmt.Comments()
	if comments == nil {
		return false, nil
	}
	if len(comments.Before) > 0 {
		if comments.Before[len(comments.Before)-1].Text == skipAnnotation {
			return true, nil
		}
	}
	allComments := [][]syntax.Comment{
		comments.Before,
		comments.Suffix,
		comments.After,
	}
	for _, c := range allComments {
		if hasAnnotation(c, skipAnnotation) {
			return false, fmt.Errorf("found skip annotation in unexpected location, it should be on the line right above the target definition to take effect")
		}
	}
	return false, nil
}

func hasAnnotation(comments []syntax.Comment, annotation string) bool {
	for _, comment := range comments {
		if comment.Text == annotation {
			return true
		}
	}
	return false
}
