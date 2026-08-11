// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"context"
	"flag"
	"fmt"
	"os"

	"github.com/google/subcommands"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/boundary"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/classify"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/discover"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/prune"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/report"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/validate"
)

type ProjectCheckCommand struct {
	fuchsiaDir string
	fileList   string
}

func (*ProjectCheckCommand) Name() string { return "check" }
func (*ProjectCheckCommand) Synopsis() string {
	return "Analyzes specific files and validates them against their parent README.fuchsia."
}
func (*ProjectCheckCommand) Usage() string {
	return `check [-file-list <path>] <files...>:
  Checks if the specified files are declared in their parent README.fuchsia.
  Use -file-list to specify a file containing paths to check, one per line.
`
}

func (c *ProjectCheckCommand) SetFlags(f *flag.FlagSet) {
	f.StringVar(&c.fileList, "file-list", "", "Path to a file containing a list of file paths to check, one per line.")
}

func (c *ProjectCheckCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	// Step 1: Load target paths and repository input context.
	inputPaths, err := LoadTargets(c.fileList, c.fuchsiaDir, f.Args())
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		return subcommands.ExitUsageError
	}

	inputCtx, err := LoadInputContext(c.fuchsiaDir, inputPaths[0])
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		return subcommands.ExitFailure
	}

	// Step 2: Initialize pipeline stages for target evaluation.
	classifier, err := classify.NewClassifier(inputCtx.Config.Classify)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to initialize classifier: %v\n", err)
		return subcommands.ExitFailure
	}

	discoverer := discover.NewCrawler(inputCtx.FuchsiaDir, inputCtx.Config.Discover)
	boundaryCfg := inputCtx.Config.Boundary
	boundaryCfg.FilesInReadmeOnly = false
	grouper := boundary.NewGrouper(inputCtx.FuchsiaDir, boundaryCfg)
	pruner := prune.NewPruner(nil)
	validator := validate.NewValidator(inputCtx.FuchsiaDir, inputCtx.Config.Validate)

	// Step 3: Iterate through input targets, running Orchestrator with Stage 6 TargetComplianceVerifier.
	cache := make(map[string]error)
	hasErrors := false

	for _, inputPath := range inputPaths {
		projectRoot, err := inputCtx.ResolveProjectRoot(inputPath)
		if err != nil {
			fmt.Fprintf(os.Stderr, "❌ Error: %v\n", err)
			hasErrors = true
			continue
		}

		if prevErr, processed := cache[projectRoot]; !processed {
			var targetProj *pipeline.Project
			verifier := report.NewTargetComplianceVerifier(inputCtx.FuchsiaDir, inputPath, inputCtx.Config)
			passPrinter := pipeline.RenderFunc(func(ctx context.Context, projects []*pipeline.Project, errors []pipeline.ComplianceError) error {
				for _, p := range projects {
					if p.RootPath == projectRoot {
						targetProj = p
						break
					}
				}
				return nil
			})

			renderers := pipeline.MultiRenderer{verifier, passPrinter}
			orchestrator := pipeline.NewOrchestrator(discoverer, grouper, pruner, classifier, validator, renderers)
			runErr := orchestrator.Run(ctx, []string{projectRoot})
			if runErr != nil {
				fmt.Fprintf(os.Stderr, "❌ Error in %s: %v\n", inputPath, runErr)
				hasErrors = true
				cache[projectRoot] = runErr
				continue
			}

			// Step 4: Output pass confirmation for compliant targets.
			if targetProj != nil && targetProj.Readme != nil {
				projectName := "Unknown Project"
				origs := targetProj.Readme.OriginalSegments()
				if len(origs) > 0 && origs[0].Name != "" {
					projectName = origs[0].Name
				} else {
					projectName = findProjectBasename(inputCtx.FuchsiaDir, inputPath, inputCtx.Config)
				}

				if inputPath == "" || inputPath == "." {
					fmt.Printf("✅ Passed: %s\n", projectName)
				} else {
					fmt.Printf("✅ Passed: %s (%s)\n", projectName, inputPath)
				}
			}

			cache[projectRoot] = nil
		} else if prevErr != nil {
			hasErrors = true
		}
	}

	// Step 5: Return overall success or failure status.
	if hasErrors {
		return subcommands.ExitFailure
	}
	return subcommands.ExitSuccess
}
