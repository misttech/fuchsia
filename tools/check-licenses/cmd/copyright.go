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
	"time"

	"github.com/google/subcommands"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/classify"
)

type CopyrightCommand struct {
	fuchsiaDir string
}

func (*CopyrightCommand) Name() string     { return "copyright" }
func (*CopyrightCommand) Synopsis() string { return "Check or add copyright headers to files." }
func (*CopyrightCommand) Usage() string {
	return `copyright <subcommand> [options]:
  Manage copyright headers.

  Subcommands:
    add   Add copyright header to file if missing.
    check Check if file has copyright header.
`
}

func (c *CopyrightCommand) SetFlags(f *flag.FlagSet) {
	f.StringVar(&c.fuchsiaDir, "fuchsia_dir", os.Getenv("FUCHSIA_DIR"), "Location of the fuchsia root directory (//).")
}

func (c *CopyrightCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	subFlags := flag.NewFlagSet("copyright", flag.ContinueOnError)
	if err := subFlags.Parse(f.Args()); err != nil {
		return subcommands.ExitUsageError
	}
	subCommander := subcommands.NewCommander(subFlags, "copyright")
	subCommander.Register(&CopyrightAddCommand{fuchsiaDir: c.fuchsiaDir}, "")
	subCommander.Register(&CopyrightCheckCommand{fuchsiaDir: c.fuchsiaDir}, "")
	return subCommander.Execute(ctx)
}

type CopyrightAddCommand struct {
	fuchsiaDir  string
	printStdout bool
}

func (*CopyrightAddCommand) Name() string     { return "add" }
func (*CopyrightAddCommand) Synopsis() string { return "Add copyright header to file if missing." }
func (*CopyrightAddCommand) Usage() string {
	return `add [-stdout] <file_path>:
  Adds a Fuchsia copyright header to the file if missing.
  Use -stdout to print result to stdout instead of modifying file.
`
}

func (c *CopyrightAddCommand) SetFlags(f *flag.FlagSet) {
	f.BoolVar(&c.printStdout, "stdout", false, "Print the formatted text to stdout instead of overwriting the file.")
}

func (c *CopyrightAddCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	if f.NArg() != 1 {
		fmt.Fprintln(os.Stderr, "Error: exactly one file path must be provided.")
		return subcommands.ExitUsageError
	}

	targetFile := f.Arg(0)
	fuchsiaDir, targetFile, err := ResolveAndValidatePath(c.fuchsiaDir, targetFile)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		return subcommands.ExitFailure
	}

	if err := ApplyCopyrightFix(fuchsiaDir, targetFile, c.printStdout); err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		return subcommands.ExitFailure
	}

	return subcommands.ExitSuccess
}

type CopyrightCheckCommand struct {
	fuchsiaDir string
}

func (*CopyrightCheckCommand) Name() string     { return "check" }
func (*CopyrightCheckCommand) Synopsis() string { return "Check if file has copyright header." }
func (*CopyrightCheckCommand) Usage() string {
	return `check <file_path>:
  Checks if the file contains the Fuchsia copyright header.
  Fails with exit code 1 if missing.
`
}

func (c *CopyrightCheckCommand) SetFlags(f *flag.FlagSet) {}

func (c *CopyrightCheckCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	if f.NArg() != 1 {
		fmt.Fprintln(os.Stderr, "Error: exactly one file path must be provided.")
		return subcommands.ExitUsageError
	}

	targetFile := f.Arg(0)
	fuchsiaDir, targetFile, err := ResolveAndValidatePath(c.fuchsiaDir, targetFile)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		return subcommands.ExitFailure
	}

	hasCopyright, err := CheckCopyright(fuchsiaDir, targetFile)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		return subcommands.ExitFailure
	}

	if !hasCopyright {
		fmt.Printf("❌ Fuchsia copyright header missing in %s\n", targetFile)
		return subcommands.ExitFailure
	}

	fmt.Printf("✅ Fuchsia copyright header found in %s\n", targetFile)
	return subcommands.ExitSuccess
}

// CheckCopyright verifies if a file has a Fuchsia copyright header.
func CheckCopyright(fuchsiaDir, filePath string) (bool, error) {
	absPath := filePath
	if !filepath.IsAbs(filePath) {
		absPath = filepath.Join(fuchsiaDir, filePath)
	}

	patternsDir := filepath.Join(fuchsiaDir, "tools", "check-licenses", "assets", "patterns")

	classifier, err := classify.NewClassifier(0.8, []string{patternsDir}, nil)
	if err != nil {
		return false, fmt.Errorf("failed to initialize classifier: %w", err)
	}

	inChan := make(chan pipeline.FilteredProject, 1)
	inChan <- pipeline.FilteredProject{
		Project: pipeline.Project{
			RootPath: filepath.Dir(absPath),
			Files:    []pipeline.FileInfo{{Path: absPath}},
		},
	}
	close(inChan)

	ctx := context.Background()
	outChan, err := classifier.Run(ctx, inChan)
	if err != nil {
		return false, fmt.Errorf("failed to run classifier: %w", err)
	}

	var result pipeline.ClassifiedFile
	for cf := range outChan {
		result = cf
	}

	for _, match := range result.Matches {
		if match.SPDXID == "FuchsiaCopyright" {
			return true, nil
		}
	}

	return false, nil
}

// ApplyCopyrightFix analyzes a file and adds a Fuchsia copyright header if missing.
func ApplyCopyrightFix(fuchsiaDir, filePath string, printStdout bool) error {
	absPath := filePath
	if !filepath.IsAbs(filePath) {
		absPath = filepath.Join(fuchsiaDir, filePath)
	}

	hasCopyright, err := CheckCopyright(fuchsiaDir, absPath)
	if err != nil {
		return err
	}

	if hasCopyright {
		if printStdout {
			content, _ := os.ReadFile(absPath)
			fmt.Print(string(content))
		} else {
			fmt.Printf("✅ Fuchsia copyright header found in %s\n", filePath)
		}
		return nil
	}

	if !printStdout {
		fmt.Printf("❌ Fuchsia copyright header missing in %s\n", filePath)
	}

	newBytes, err := addCopyright(absPath)
	if err != nil {
		return err
	}

	if printStdout {
		fmt.Print(string(newBytes))
		return nil
	}

	if err := os.WriteFile(absPath, newBytes, 0644); err != nil {
		return fmt.Errorf("failed to write file: %w", err)
	}
	fmt.Printf("✏️  Successfully added Fuchsia copyright header to %s\n", filePath)

	return nil
}

var commentPrefixes = map[string]string{
	// C-style comments
	".c": "//", ".cc": "//", ".cpp": "//", ".h": "//", ".hh": "//", ".hpp": "//",
	".inc": "//", ".go": "//", ".rs": "//", ".dart": "//", ".java": "//", ".js": "//",
	".m": "//", ".cml": "//", ".fidl": "//", ".d": "//", ".dat": "//",
	// Script/Config-style comments
	".py": "#", ".sh": "#", ".gn": "#", ".gni": "#", ".gyp": "#", ".gypi": "#",
	".merkle": "#", ".ac": "#", ".am": "#",
	// Assembly
	".asm": ";",
	// Windows Batch
	".bat": "rem",
}

func addCopyright(filePath string) ([]byte, error) {
	ext := filepath.Ext(filePath)

	commentPrefix, ok := commentPrefixes[ext]
	if !ok {
		// Do not guess. If we guess wrong, we corrupt the build.
		return nil, fmt.Errorf("unsupported file extension %q for automatic copyright injection", ext)
	}

	year := time.Now().Year()

	var header string
	if commentPrefix == "rem" {
		header = fmt.Sprintf("%s Copyright %d The Fuchsia Authors. All rights reserved.\n%s Use of this source code is governed by a BSD-style license that can be\n%s found in the LICENSE file.\n\n", commentPrefix, year, commentPrefix, commentPrefix)
	} else {
		header = fmt.Sprintf("%s Copyright %d The Fuchsia Authors. All rights reserved.\n%s Use of this source code is governed by a BSD-style license that can be\n%s found in the LICENSE file.\n\n", commentPrefix, year, commentPrefix, commentPrefix)
	}

	content, err := os.ReadFile(filePath)
	if err != nil {
		return nil, err
	}

	// Prepend the header
	var newContent bytes.Buffer

	// Handle shebangs (e.g. #!/bin/bash)
	if bytes.HasPrefix(content, []byte("#!")) {
		lines := bytes.SplitN(content, []byte("\n"), 2)
		newContent.Write(lines[0])
		newContent.WriteString("\n\n")
		newContent.WriteString(header)
		if len(lines) > 1 {
			newContent.Write(lines[1])
		}
	} else {
		newContent.WriteString(header)
		newContent.Write(content)
	}

	return newContent.Bytes(), nil
}
