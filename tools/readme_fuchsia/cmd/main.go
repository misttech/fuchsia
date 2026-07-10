// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"errors"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"go.fuchsia.dev/fuchsia/tools/readme_fuchsia"
)

func printUsage() {
	fmt.Fprintf(os.Stderr, "Usage: %s <command> [args...]\n", os.Args[0])
	fmt.Fprintf(os.Stderr, "Commands: validate, format, get, set, help\n")
}

func main() {
	if len(os.Args) < 2 {
		printUsage()
		os.Exit(1)
	}

	command := os.Args[1]
	switch command {
	case "help", "-h", "--help":
		printUsage()
		os.Exit(0)
	case "validate":
		if err := runValidate(os.Args[2:]); err != nil {
			if err.Error() != "" {
				fmt.Fprintln(os.Stderr, err)
			}
			os.Exit(1)
		}
	case "format":
		if err := runFormat(os.Args[2:]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
	case "get":
		if err := runGet(os.Args[2:]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
	case "set":
		if err := runSet(os.Args[2:]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
	default:
		fmt.Fprintf(os.Stderr, "Unknown command: %s\n", command)
		printUsage()
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
		fmt.Fprintf(os.Stderr, "validation failed for %s\n", readmePath)
		for _, err := range errs {
			fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		}
		return errors.New("")
	}

	fmt.Printf("Validation passed for %s\n", readmePath)
	return nil
}

func runFormat(args []string) error {
	fs := flag.NewFlagSet("format", flag.ExitOnError)
	stdout := fs.Bool("stdout", false, "Print formatted content to stdout instead of modifying the file in-place")
	fs.Parse(args)

	if fs.NArg() > 1 {
		return fmt.Errorf("usage: format [--stdout] [path/to/README.fuchsia]")
	}

	readmePath := "README.fuchsia"
	if fs.NArg() == 1 {
		readmePath = fs.Arg(0)
	}

	readmes, err := readme_fuchsia.ParseFile(readmePath)
	if err != nil {
		return fmt.Errorf("failed to parse %s: %w", readmePath, err)
	}

	formatted := readme_fuchsia.Format(readmes)

	if *stdout {
		fmt.Print(formatted)
		return nil
	}

	err = os.WriteFile(readmePath, []byte(formatted), 0644)
	if err != nil {
		return fmt.Errorf("failed to write formatted content to %s: %w", readmePath, err)
	}

	fmt.Printf("Formatted %s\n", readmePath)
	return nil
}

func runGet(args []string) error {
	fs := flag.NewFlagSet("get", flag.ExitOnError)
	fs.Parse(args)

	if fs.NArg() < 1 || fs.NArg() > 2 {
		return fmt.Errorf("usage: get <field> [path/to/README.fuchsia]")
	}

	field := fs.Arg(0)
	readmePath := "README.fuchsia"
	if fs.NArg() == 2 {
		readmePath = fs.Arg(1)
	}

	readmes, err := readme_fuchsia.ParseFile(readmePath)
	if err != nil {
		return fmt.Errorf("failed to parse %s: %w", readmePath, err)
	}

	if len(readmes) == 0 {
		return fmt.Errorf("no projects found in %s", readmePath)
	}

	// Default to the first project
	readme := readmes[0]
	val, ok := readme.GetField(field)
	if !ok {
		return fmt.Errorf("field %q not found in %s", field, readmePath)
	}

	fmt.Println(val)
	return nil
}

func runSet(args []string) error {
	fs := flag.NewFlagSet("set", flag.ExitOnError)
	fs.Parse(args)

	if fs.NArg() < 2 || fs.NArg() > 3 {
		return fmt.Errorf("usage: set <field> <value> [path/to/README.fuchsia]")
	}

	field := fs.Arg(0)
	value := fs.Arg(1)
	readmePath := "README.fuchsia"
	if fs.NArg() == 3 {
		readmePath = fs.Arg(2)
	}

	readmes, err := readme_fuchsia.ParseFile(readmePath)
	if err != nil {
		if os.IsNotExist(err) {
			readmes = []*readme_fuchsia.Readme{}
		} else {
			return fmt.Errorf("failed to parse %s: %w", readmePath, err)
		}
	}

	if len(readmes) == 0 {
		readmes = append(readmes, &readme_fuchsia.Readme{})
	}

	// Default to the first project
	readme := readmes[0]
	err = readme.SetField(field, value)
	if err != nil {
		return fmt.Errorf("failed to set field %q: %w", field, err)
	}

	formatted := readme_fuchsia.Format(readmes)
	err = os.WriteFile(readmePath, []byte(formatted), 0644)
	if err != nil {
		return fmt.Errorf("failed to write to %s: %w", readmePath, err)
	}

	return nil
}
