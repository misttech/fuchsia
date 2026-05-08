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

type ReadmeCommand struct{}

func (*ReadmeCommand) Name() string     { return "readme" }
func (*ReadmeCommand) Synopsis() string { return "Format or check a README.fuchsia file." }
func (*ReadmeCommand) Usage() string {
	return `readme <subcommand> [options]:
  Manage README.fuchsia files.

  Subcommands:
    format   Formats the README.fuchsia file in-place.
    check    Checks if the README.fuchsia file is valid and formatted.
`
}

func (c *ReadmeCommand) SetFlags(f *flag.FlagSet) {}

func (c *ReadmeCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	subFlags := flag.NewFlagSet("readme", flag.ContinueOnError)
	if err := subFlags.Parse(f.Args()); err != nil {
		return subcommands.ExitUsageError
	}
	subCommander := subcommands.NewCommander(subFlags, "readme")
	subCommander.Register(&ReadmeFormatCommand{}, "")
	subCommander.Register(&ReadmeCheckCommand{}, "")
	return subCommander.Execute(ctx)
}

type ReadmeFormatCommand struct {
	printStdout bool
}

func (*ReadmeFormatCommand) Name() string     { return "format" }
func (*ReadmeFormatCommand) Synopsis() string { return "Formats the README.fuchsia file in-place." }
func (*ReadmeFormatCommand) Usage() string {
	return `format [-stdout] <file_path>:
  Formats the README.fuchsia file to match the canonical schema.
  Use -stdout to print the formatted text to stdout without modifying the file.
`
}

func (c *ReadmeFormatCommand) SetFlags(f *flag.FlagSet) {
	f.BoolVar(&c.printStdout, "stdout", false, "Print the formatted text to stdout instead of overwriting the file.")
}

func (c *ReadmeFormatCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	if f.NArg() != 1 {
		fmt.Fprintln(os.Stderr, "Error: exactly one file path must be provided.")
		return subcommands.ExitUsageError
	}

	targetFile := f.Arg(0)
	fuchsiaDir, targetFile, err := ResolveAndValidatePath("", targetFile)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		return subcommands.ExitFailure
	}

	absPath := filepath.Join(fuchsiaDir, targetFile)

	data, err := os.ReadFile(absPath)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to read file: %v\n", err)
		return subcommands.ExitFailure
	}

	readmes, err := readme.Parse(data)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to parse README: %v\n", err)
		return subcommands.ExitFailure
	}

	if len(readmes) == 0 {
		fmt.Fprintf(os.Stderr, "Warning: parsed 0 readmes from file\n")
		return subcommands.ExitSuccess
	}

	formattedText := readme.Format(readmes)
	formattedBytes := []byte(formattedText)

	errs := readme.Validate(readmes)
	hasValidationErrors := len(errs) > 0
	for _, err := range errs {
		fmt.Fprintf(os.Stderr, "❌ Error: %v\n", err)
	}

	if c.printStdout {
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

type ReadmeCheckCommand struct{}

func (*ReadmeCheckCommand) Name() string { return "check" }
func (*ReadmeCheckCommand) Synopsis() string {
	return "Checks if the README.fuchsia file is valid and formatted."
}
func (*ReadmeCheckCommand) Usage() string {
	return `check <file_path>:
  Checks if the README.fuchsia file is perfectly formatted and contains no unknown fields.
`
}

func (c *ReadmeCheckCommand) SetFlags(f *flag.FlagSet) {}

func (c *ReadmeCheckCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	if f.NArg() != 1 {
		fmt.Fprintln(os.Stderr, "Error: exactly one file path must be provided.")
		return subcommands.ExitUsageError
	}

	targetFile := f.Arg(0)
	fuchsiaDir, targetFile, err := ResolveAndValidatePath("", targetFile)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		return subcommands.ExitFailure
	}

	absPath := filepath.Join(fuchsiaDir, targetFile)

	data, err := os.ReadFile(absPath)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to read file: %v\n", err)
		return subcommands.ExitFailure
	}

	readmes, err := readme.Parse(data)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to parse README: %v\n", err)
		return subcommands.ExitFailure
	}

	if len(readmes) == 0 {
		fmt.Fprintf(os.Stderr, "Warning: parsed 0 readmes from file\n")
		return subcommands.ExitSuccess
	}

	formattedText := readme.Format(readmes)
	formattedBytes := []byte(formattedText)

	errs := readme.Validate(readmes)
	hasValidationErrors := len(errs) > 0
	for _, err := range errs {
		fmt.Fprintf(os.Stderr, "❌ Error: %v\n", err)
	}

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
