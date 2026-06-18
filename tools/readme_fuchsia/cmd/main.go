// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"go.fuchsia.dev/fuchsia/tools/readme_fuchsia"
)

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintf(os.Stderr, "Usage: %s <command> [args...]\n", os.Args[0])
		fmt.Fprintf(os.Stderr, "Commands: validate\n")
		os.Exit(1)
	}

	command := os.Args[1]
	switch command {
	case "validate":
		if err := runValidate(os.Args[2:]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
	default:
		fmt.Fprintf(os.Stderr, "Unknown command: %s\n", command)
		os.Exit(1)
	}
}

func runValidate(args []string) error {
	fs := flag.NewFlagSet("validate", flag.ExitOnError)
	projectRoot := fs.String("project-root", "", "Optional override for the project's physical location")
	allowMissingLicense := fs.Bool("allow-missing-license", false, "Allow missing license/license file")

	fs.Parse(args)

	if fs.NArg() != 1 {
		return fmt.Errorf("usage: validate [--project-root <dir>] [--allow-missing-license] <path/to/README.fuchsia>")
	}

	readmePath := fs.Arg(0)

	readmes, err := readme_fuchsia.ParseFile(readmePath)
	if err != nil {
		return fmt.Errorf("failed to parse %s: %w", readmePath, err)
	}

	root := *projectRoot
	if root == "" {
		root = filepath.Dir(readmePath)
	}

	errs := readme_fuchsia.Validate(root, readmes)
	if *allowMissingLicense && len(errs) > 0 {
		var filteredErrs []error
		for _, err := range errs {
			msg := err.Error()
			if strings.Contains(msg, "Missing required field 'License'") || strings.Contains(msg, "Missing required field 'License File'") {
				continue
			}
			filteredErrs = append(filteredErrs, err)
		}
		errs = filteredErrs
	}
	if len(errs) > 0 {
		for _, err := range errs {
			fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		}
		return fmt.Errorf("validation failed for %s", readmePath)
	}

	fmt.Printf("Validation passed for %s\n", readmePath)
	return nil
}
