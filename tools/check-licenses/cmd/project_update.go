// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"context"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"time"

	"github.com/google/subcommands"

	v2config "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/config"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/readme"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/classify"
)

type ProjectUpdateCommand struct {
	fuchsiaDir  string
	printStdout bool
}

func (*ProjectUpdateCommand) Name() string { return "update" }
func (*ProjectUpdateCommand) Synopsis() string {
	return "Automatically updates the License File declarations in a README.fuchsia."
}
func (*ProjectUpdateCommand) Usage() string {
	return `update [-stdout] <dir>:
  Automatically updates the License File declarations in a README.fuchsia.
  Use -stdout to print result to stdout instead of overwriting file.
`
}

func (c *ProjectUpdateCommand) SetFlags(f *flag.FlagSet) {
	f.BoolVar(&c.printStdout, "stdout", false, "Print the updated README to stdout instead of overwriting the file.")
}

func (c *ProjectUpdateCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	if f.NArg() != 1 {
		fmt.Fprintln(os.Stderr, "Error: exactly one directory path must be provided.")
		return subcommands.ExitUsageError
	}
	targetDir := f.Arg(0)
	fuchsiaDir, targetPath, err := ResolveAndValidatePath(c.fuchsiaDir, targetDir)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		return subcommands.ExitFailure
	}
	absDir := filepath.Join(fuchsiaDir, targetPath)

	builder := v2config.NewBuilder(fuchsiaDir)
	if err := builder.Assemble(); err != nil {
		fmt.Fprintf(os.Stderr, "Failed to assemble configuration: %v\n", err)
		return subcommands.ExitFailure
	}
	config := builder.Config

	patternsDir := filepath.Join(fuchsiaDir, "tools", "check-licenses", "assets", "patterns")
	classifier, err := classify.NewClassifier(0.8, []string{patternsDir}, config.TargetExtensions)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to initialize classifier: %v\n", err)
		return subcommands.ExitFailure
	}

	startTime := time.Now()
	_, updatedReadmes, readmePath, targetProject, filesToClassify, foundLicenses, err := RunProjectPipeline(ctx, fuchsiaDir, absDir, config, classifier)
	if err != nil {
		fmt.Fprintf(os.Stderr, "%v\n", err)
		return subcommands.ExitFailure
	}

	formatted := readme.Format(updatedReadmes)

	if c.printStdout {
		fmt.Print(formatted)
	} else {
		if err := os.WriteFile(readmePath, []byte(formatted), 0644); err != nil {
			fmt.Fprintf(os.Stderr, "Failed to write README.fuchsia: %v\n", err)
			return subcommands.ExitFailure
		}
		fmt.Printf("✏️  Successfully updated %s\n", readmePath)

		duration := time.Since(startTime)
		fmt.Printf("\nUpdate Summary for %s\n", targetDir)
		fmt.Printf("----------------------------------------------------\n")
		fmt.Printf("Files discovered in project: %d\n", len(targetProject.Files))
		fmt.Printf("Files classified:            %d\n", len(filesToClassify))
		fmt.Printf("Files containing licenses:   %d\n", len(foundLicenses))
		fmt.Printf("Total execution time:        %v\n", duration)
	}

	return subcommands.ExitSuccess
}
