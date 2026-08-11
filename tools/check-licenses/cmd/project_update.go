// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"context"
	"flag"
	"fmt"
	"os"
	"time"

	"github.com/google/subcommands"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/boundary"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/classify"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/discover"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/prune"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/report"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/validate"
)

type ProjectUpdateCommand struct {
	fuchsiaDir  string
	printStdout bool
	fileList    string
}

func (*ProjectUpdateCommand) Name() string { return "update" }
func (*ProjectUpdateCommand) Synopsis() string {
	return "Automatically updates the License File declarations in a README.fuchsia."
}
func (*ProjectUpdateCommand) Usage() string {
	return `update [-stdout] [-file-list <path>] <paths...>:
  Automatically updates the License File declarations in a README.fuchsia.
  Use -stdout to print result to stdout instead of overwriting file.
  Use -file-list to specify a file containing paths to update, one per line.
`
}

func (c *ProjectUpdateCommand) SetFlags(f *flag.FlagSet) {
	f.BoolVar(&c.printStdout, "stdout", false, "Print the updated README to stdout instead of overwriting the file.")
	f.StringVar(&c.fileList, "file-list", "", "Path to a file containing a list of file paths to update, one per line.")
}

func (c *ProjectUpdateCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
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

	// Step 2: Initialize pipeline stages for project updating.
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

	// Step 3: Iterate through target project roots, executing Orchestrator with Stage 6 ReadmeWriter.
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
			startTime := time.Now()

			var renderers pipeline.MultiRenderer
			renderers = append(renderers, report.NewReadmeWriter(inputCtx.FuchsiaDir, c.printStdout))
			if !c.printStdout {
				renderers = append(renderers, report.NewSummaryMetricsPrinter(inputCtx.FuchsiaDir, startTime))
			}

			orchestrator := pipeline.NewOrchestrator(discoverer, grouper, pruner, classifier, validator, renderers)
			if err := orchestrator.Run(ctx, []string{projectRoot}); err != nil {
				fmt.Fprintf(os.Stderr, "Failed to update project %s: %v\n", inputPath, err)
				hasErrors = true
				cache[projectRoot] = err
				continue
			}
			cache[projectRoot] = nil
		} else if prevErr != nil {
			hasErrors = true
		}
	}

	// Step 4: Return overall success or failure status.
	if hasErrors {
		return subcommands.ExitFailure
	}
	return subcommands.ExitSuccess
}
