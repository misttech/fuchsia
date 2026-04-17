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

	"github.com/google/subcommands"

	v2config "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/config"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/classify"
)

type ProjectCommand struct {
	fuchsiaDir  string
	printStdout bool
}

func (*ProjectCommand) Name() string     { return "project" }
func (*ProjectCommand) Synopsis() string { return "Check or update project metadata and compliance." }
func (*ProjectCommand) Usage() string {
	return `project <action> [options] <args...>:
  Actions:
    check <files...>   Analyzes specific files and validates them against their parent README.fuchsia.
    info <path>        Shows the project metadata and compliance state for a given file or directory.
    list <dir>         Lists all project boundaries discovered under the given directory.
    update <dir>       Automatically updates the License File declarations in a README.fuchsia.
`
}

func (c *ProjectCommand) SetFlags(f *flag.FlagSet) {
	f.StringVar(&c.fuchsiaDir, "fuchsia_dir", os.Getenv("FUCHSIA_DIR"), "Location of the fuchsia root directory (//).")
	f.BoolVar(&c.printStdout, "stdout", false, "Print the updated README to stdout instead of overwriting the file (for 'update').")
}

func (c *ProjectCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	if f.NArg() < 1 {
		fmt.Fprintln(os.Stderr, "Error: action ('check', 'info', 'list', 'update') must be provided.")
		return subcommands.ExitUsageError
	}

	action := f.Arg(0)
	if action != "check" && action != "info" && action != "list" && action != "update" {
		fmt.Fprintf(os.Stderr, "Error: unknown action '%s'. Expected 'check', 'info', 'list', or 'update'.\n", action)
		return subcommands.ExitUsageError
	}

	if c.fuchsiaDir == "" {
		c.fuchsiaDir = "."
	}

	absFuchsiaDir, err := filepath.Abs(c.fuchsiaDir)
	if err == nil {
		c.fuchsiaDir = absFuchsiaDir
	}

	builder := v2config.NewBuilder(c.fuchsiaDir)
	if err := builder.Assemble(); err != nil {
		fmt.Fprintf(os.Stderr, "Failed to assemble configuration: %v\n", err)
		return subcommands.ExitFailure
	}
	config := builder.Config

	patternsDir := filepath.Join(c.fuchsiaDir, "tools", "check-licenses", "assets", "patterns")
	classifier, err := classify.NewClassifier(0.8, []string{patternsDir}, config.TargetExtensions)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to initialize classifier: %v\n", err)
		return subcommands.ExitFailure
	}

	switch action {
	case "list":
		return c.executeList(ctx, f.Args()[1:], config)
	case "info":
		return c.executeInfo(ctx, f.Args()[1:], config)
	case "update":
		return c.executeUpdate(ctx, f.Args()[1:], config, classifier)
	case "check":
		if f.NArg() < 2 {
			fmt.Fprintln(os.Stderr, "Error: 'check' requires at least one file path.")
			return subcommands.ExitUsageError
		}
		hasErrors := false
		for _, targetFile := range f.Args()[1:] {
			absPath, err := filepath.Abs(targetFile)
			if err != nil {
				fmt.Fprintf(os.Stderr, "Failed to resolve absolute path for %s: %v\n", targetFile, err)
				hasErrors = true
				continue
			}

			if err := c.checkFile(ctx, absPath, config, classifier); err != nil {
				fmt.Fprintf(os.Stderr, "❌ Error in %s: %v\n", targetFile, err)
				hasErrors = true
			} else {
				fmt.Printf("✅ Passed: %s\n", targetFile)
			}
		}
		if hasErrors {
			return subcommands.ExitFailure
		}
		return subcommands.ExitSuccess
	}
	return subcommands.ExitUsageError
}

// runClassifierOnFile is a helper to orchestrate a single-file pipeline run.
func runClassifierOnFile(ctx context.Context, classifier *classify.Classifier, absPath string) (*pipeline.ClassifiedFile, error) {
	inChan := make(chan pipeline.FilteredProject, 1)
	inChan <- pipeline.FilteredProject{
		Project: pipeline.Project{
			RootPath: filepath.Dir(absPath),
			Files:    []pipeline.FileInfo{{Path: absPath, LicenseParser: "Single License"}},
		},
	}
	close(inChan)

	outChan, err := classifier.Run(ctx, inChan)
	if err != nil {
		return nil, fmt.Errorf("failed to run classifier: %w", err)
	}

	var cf pipeline.ClassifiedFile
	for result := range outChan {
		cf = result
	}
	return &cf, nil
}
