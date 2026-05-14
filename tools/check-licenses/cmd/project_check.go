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
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/readme"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/classify"
)

type ProjectCheckCommand struct {
	fuchsiaDir string
}

func (*ProjectCheckCommand) Name() string { return "check" }
func (*ProjectCheckCommand) Synopsis() string {
	return "Analyzes specific files and validates them against their parent README.fuchsia."
}
func (*ProjectCheckCommand) Usage() string {
	return `check <files...>:
  Checks if the specified files are declared in their parent README.fuchsia.
`
}

func (c *ProjectCheckCommand) SetFlags(f *flag.FlagSet) {}

func (c *ProjectCheckCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	if f.NArg() < 1 {
		fmt.Fprintln(os.Stderr, "Error: at least one file path must be provided.")
		return subcommands.ExitUsageError
	}

	fuchsiaDir, _, err := ResolveAndValidatePath(c.fuchsiaDir, f.Arg(0))
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		return subcommands.ExitFailure
	}

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

	hasErrors := false
	for _, targetPath := range f.Args() {
		fuchsiaDir, relTargetPath, err := ResolveAndValidatePath(c.fuchsiaDir, targetPath)
		if err != nil {
			fmt.Fprintf(os.Stderr, "Error: %v\n", err)
			hasErrors = true
			continue
		}
		absPath := filepath.Join(fuchsiaDir, relTargetPath)
		info, err := os.Stat(absPath)
		if err != nil {
			fmt.Fprintf(os.Stderr, "❌ Error: path does not exist: %s\n", targetPath)
			hasErrors = true
			continue
		}

		var projectRoot string
		if info.IsDir() {
			projectRoot = absPath
		} else {
			r, bestReadmePath, err := readme.FindProjectReadme(absPath, fuchsiaDir, config.OutOfTreeReadmes)
			if err == nil && bestReadmePath != "" && r != nil {
				logicalDir := filepath.Dir(bestReadmePath)
				for logPath, physPath := range config.OutOfTreeReadmes {
					if physPath == bestReadmePath {
						logicalDir = filepath.Join(fuchsiaDir, logPath)
						break
					}
				}
				projectRoot = logicalDir
			} else {
				projectRoot = filepath.Dir(absPath)
			}
		}

		originalReadmes, updatedReadmes, readmePath, _, _, _, err := RunProjectPipeline(ctx, fuchsiaDir, projectRoot, config, classifier)
		if err != nil {
			fmt.Fprintf(os.Stderr, "❌ Error analyzing project %s: %v\n", targetPath, err)
			hasErrors = true
			continue
		}

		if readmeErrs := readme.Validate(fuchsiaDir, readmePath, originalReadmes, config); len(readmeErrs) > 0 {
			for _, rErr := range readmeErrs {
				fmt.Fprintf(os.Stderr, "❌ Error in %s: %v\n", targetPath, rErr)
			}
			hasErrors = true
			continue
		}

		if err := verifyTargetCompliance(originalReadmes, updatedReadmes, absPath, projectRoot, info.IsDir()); err != nil {
			relTarget, _ := filepath.Rel(fuchsiaDir, absPath)
			if relTarget == "." {
				relTarget = targetPath
			}
			fmt.Fprintf(os.Stderr, "❌ Error in %s: %v\n", relTarget, err)
			hasErrors = true
		} else {
			projectName := "Unknown Project"
			if len(originalReadmes) > 0 && originalReadmes[0].Name != "" {
				projectName = originalReadmes[0].Name
			} else {
				projectName = findProjectBasename(relTargetPath, config.ManifestProjectNames)
			}

			if relTargetPath == "" || relTargetPath == "." {
				fmt.Printf("✅ Passed: %s\n", projectName)
			} else {
				fmt.Printf("✅ Passed: %s (%s)\n", projectName, targetPath)
			}
		}
	}

	if hasErrors {
		return subcommands.ExitFailure
	}
	return subcommands.ExitSuccess
}
