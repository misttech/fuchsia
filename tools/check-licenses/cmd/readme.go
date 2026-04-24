// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"bytes"
	"context"
	"flag"
	"fmt"
	"os"
	"path/filepath"

	"github.com/google/subcommands"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/readme"
)

type ReadmeCommand struct {
	printStdout bool
}

func (*ReadmeCommand) Name() string     { return "readme" }
func (*ReadmeCommand) Synopsis() string { return "Format or check a README.fuchsia file." }
func (*ReadmeCommand) Usage() string {
	return `readme <action> <file_path>:
  Actions:
    format   Formats the README.fuchsia file in-place to match the canonical schema.
             Use -stdout to print the formatted text to stdout without modifying the file.
    check    Checks if the README.fuchsia file is perfectly formatted, contains no unknown fields,
             and has all required fields (Name, URL, Version, License File, Security Critical).

  Examples:
    fx check-licenses readme format third_party/foo/README.fuchsia
    fx check-licenses readme format -stdout third_party/foo/README.fuchsia
    fx check-licenses readme check third_party/foo/README.fuchsia
`
}

func (c *ReadmeCommand) SetFlags(f *flag.FlagSet) {
	f.BoolVar(&c.printStdout, "stdout", false, "Print the formatted text to stdout instead of overwriting the file.")
}

func (c *ReadmeCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	if f.NArg() != 2 {
		fmt.Fprintln(os.Stderr, "Error: exactly one action ('format' or 'check') and one file path must be provided.")
		return subcommands.ExitUsageError
	}

	action := f.Arg(0)
	targetFile := f.Arg(1)

	if action != "format" && action != "check" {
		fmt.Fprintf(os.Stderr, "Error: unknown action '%s'. Expected 'format' or 'check'.\n", action)
		return subcommands.ExitUsageError
	}

	absPath, err := filepath.Abs(targetFile)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to resolve absolute path for %s: %v\n", targetFile, err)
		return subcommands.ExitFailure
	}

	// 1. Read the file
	data, err := os.ReadFile(absPath)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to read file: %v\n", err)
		return subcommands.ExitFailure
	}

	// 2. Parse the file into our structured Readme slice
	readmes, err := readme.Parse(data)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to parse README: %v\n", err)
		return subcommands.ExitFailure
	}

	if len(readmes) == 0 {
		fmt.Fprintf(os.Stderr, "Warning: parsed 0 readmes from file (file might be empty)\n")
		return subcommands.ExitSuccess
	}

	// 3. Format it back to canonical text
	formattedText := readme.Format(readmes)
	formattedBytes := []byte(formattedText)

	// Run Validation Checks for both 'check' and 'format'
	hasValidationErrors := false
	for i, r := range readmes {
		// Check 1: Unknown fields
		if len(r.UnknownFields) > 0 {
			fmt.Fprintf(os.Stderr, "❌ Error (Readme %d): Found unknown/invalid fields: %+v\n", i+1, r.UnknownFields)
			hasValidationErrors = true
		}

		// Check 2: Required Fields
		if r.Name == "" {
			fmt.Fprintf(os.Stderr, "❌ Error (Readme %d): 'Name' is a required field.\n", i+1)
			hasValidationErrors = true
		}
		if r.URL == "" {
			fmt.Fprintf(os.Stderr, "❌ Error (Readme %d): 'URL' is a required field.\n", i+1)
			hasValidationErrors = true
		}
		if r.Version == "" {
			fmt.Fprintf(os.Stderr, "❌ Error (Readme %d): 'Version' is a required field.\n", i+1)
			hasValidationErrors = true
		}
		if r.SecurityCritical != "yes" && r.SecurityCritical != "no" {
			fmt.Fprintf(os.Stderr, "❌ Error (Readme %d): 'Security Critical' is required and must be exactly 'yes' or 'no'. Got: %q\n", i+1, r.SecurityCritical)
			hasValidationErrors = true
		}
		if i > 0 && r.Location == "" {
			fmt.Fprintf(os.Stderr, "❌ Error (Readme %d): 'Location' is a required field for sub-projects defined after a DEPENDENCY DIVIDER.\n", i+1)
			hasValidationErrors = true
		}
		if len(r.LicenseFiles) == 0 {
			fmt.Fprintf(os.Stderr, "❌ Error (Readme %d): At least one 'License File' must be specified.\n", i+1)
			hasValidationErrors = true
		} else {
			for _, lf := range r.LicenseFiles {
				if lf.License == "" {
					fmt.Fprintf(os.Stderr, "❌ Error (Readme %d): License File '%s' is missing required '  License:' metadata.\n", i+1, lf.Path)
					hasValidationErrors = true
				}
			}
		}
	}

	// Action: Check
	if action == "check" {
		// Check 3: Formatting consistency
		// Trimming whitespace to avoid complaining about trailing newlines on an otherwise perfect file
		if !bytes.Equal(bytes.TrimSpace(data), bytes.TrimSpace(formattedBytes)) {
			fmt.Fprintf(os.Stderr, "❌ Error: File is not canonically formatted. Run 'fx check-licenses readme format %s' to fix it.\n", targetFile)
			hasValidationErrors = true
		}

		if hasValidationErrors {
			return subcommands.ExitFailure
		}

		fmt.Printf("✅ README is perfectly formatted and contains no unknown fields: %s\n", targetFile)
		return subcommands.ExitSuccess
	}

	// Action: Format
	if action == "format" {
		if c.printStdout {
			// Print to stdout (required for SHAC formatters)
			fmt.Print(formattedText)
			if hasValidationErrors {
				return subcommands.ExitFailure
			}
			return subcommands.ExitSuccess
		}

		if bytes.Equal(data, formattedBytes) {
			if hasValidationErrors {
				return subcommands.ExitFailure
			}
			fmt.Printf("✅ README is already formatted: %s\n", targetFile)
			return subcommands.ExitSuccess
		}

		if err := os.WriteFile(absPath, formattedBytes, 0644); err != nil {
			fmt.Fprintf(os.Stderr, "Failed to write formatted README: %v\n", err)
			return subcommands.ExitFailure
		}

		if hasValidationErrors {
			return subcommands.ExitFailure
		}
		fmt.Printf("✏️  Successfully formatted README: %s\n", targetFile)
		return subcommands.ExitSuccess
	}

	return subcommands.ExitFailure
}
