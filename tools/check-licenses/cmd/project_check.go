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
	"sort"
	"strings"

	"github.com/google/subcommands"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/pipeline"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/readme"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/stages/boundary"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/stages/classify"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/stages/discover"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/stages/prune"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/stages/report"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/stages/validate"
)

type ProjectCheckCommand struct {
	fuchsiaDir string
	fileList   string
	fast       bool
}

func (*ProjectCheckCommand) Name() string { return "check" }
func (*ProjectCheckCommand) Synopsis() string {
	return "Analyzes specific files and validates them against their parent README.fuchsia."
}
func (*ProjectCheckCommand) Usage() string {
	return `check [--fast] [-file-list <path>] <files...>:
  Checks if the specified files are declared in their parent README.fuchsia.
  Use -file-list to specify a file containing paths to check, one per line.
  Use --fast to only evaluate declared license files and explicit targets.
`
}

func (c *ProjectCheckCommand) SetFlags(f *flag.FlagSet) {
	f.StringVar(&c.fileList, "file-list", "", "Path to a file containing a list of file paths to check, one per line.")
	f.BoolVar(&c.fast, "fast", false, "Fast mode: only check files declared in README.fuchsia and target paths, avoiding full directory recursion.")
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

	boundaryCfg := inputCtx.Config.Boundary
	boundaryCfg.FilesInReadmeOnly = false
	validator := validate.NewValidator(inputCtx.FuchsiaDir, inputCtx.Config.Validate)

	// Step 3: Group input targets by project root.
	// We map each input target path to its enclosing project root so that multiple files belonging to
	// the same project are verified together against their governing README in a single pipeline run.
	projectTargets := make(map[string][]string)
	hasErrors := false

	for _, inputPath := range inputPaths {
		projectRoot, err := inputCtx.ResolveProjectRoot(inputPath)
		if err != nil {
			fmt.Fprintf(os.Stderr, "❌ Error: %v\n", err)
			hasErrors = true
			continue
		}
		projectTargets[projectRoot] = append(projectTargets[projectRoot], inputPath)
	}

	// Sort project roots to ensure deterministic execution ordering.
	var projectRoots []string
	for root := range projectTargets {
		projectRoots = append(projectRoots, root)
	}
	sort.Strings(projectRoots)

	for _, projectRoot := range projectRoots {
		targets := projectTargets[projectRoot]

		var targetProj *pipeline.Project
		// TargetComplianceVerifier checks that individual target files or sub-directories
		// have their detected licenses declared in their governing README.fuchsia.
		verifier := report.NewTargetComplianceVerifier(inputCtx.FuchsiaDir, inputCtx.Config, targets...)
		// Capture the target project after grouping and pruning to report its name in the pass confirmation.
		passPrinter := pipeline.RenderFunc(func(ctx context.Context, projects []*pipeline.Project, errors []pipeline.ComplianceError) error {
			for _, p := range projects {
				if p.RootPath == projectRoot {
					targetProj = p
					break
				}
			}
			return nil
		})

		disc := discover.NewCrawler(inputCtx.FuchsiaDir, inputCtx.Config.Discover)
		grouper := boundary.NewGrouper(inputCtx.FuchsiaDir, boundaryCfg)

		// When fast mode is requested, configure the pruner to keep only the files explicitly
		// declared in the governing README or explicitly targeted, skipping unnecessary files.
		iterPruner := prune.NewPruner(nil)
		iterPruner.FilesInReadmeOnly = c.fast
		iterPruner.TargetFiles = targets
		iterPruner.FuchsiaDir = inputCtx.FuchsiaDir

		// If the target path points to a subdirectory of the project, widen crawlRoot to the directory
		// containing the governing README so that the README and its notices can be found and verified.
		crawlRoot := projectRoot
		if _, bestReadmePath, err := readme.FindProjectReadme(projectRoot, inputCtx.FuchsiaDir, inputCtx.Config.Boundary.OutOfTreeReadmes); err == nil && bestReadmePath != "" && strings.HasPrefix(bestReadmePath, inputCtx.FuchsiaDir) {
			readmeDir := filepath.Dir(bestReadmePath)
			if strings.HasPrefix(projectRoot, readmeDir) {
				crawlRoot = readmeDir
			}
		}

		// For first-party code, the project root is the entire Fuchsia repository (FuchsiaDir).
		// When --fast is not specified, avoid walking the entire repository by scoping the crawler's
		// roots to the specific directories containing the targeted files.
		var crawlRoots []string
		if projectRoot == inputCtx.FuchsiaDir && !c.fast {
			targetDirs := make(map[string]bool)
			for _, t := range targets {
				if info, err := os.Stat(t); err == nil && info.IsDir() {
					targetDirs[t] = true
				} else {
					targetDirs[filepath.Dir(t)] = true
				}
			}
			for td := range targetDirs {
				crawlRoots = append(crawlRoots, td)
			}
			sort.Strings(crawlRoots)
		} else {
			crawlRoots = []string{crawlRoot}
		}

		// Execute the pipeline on crawlRoots to discover, group, prune, classify, and verify compliance.
		renderers := pipeline.MultiRenderer{verifier, passPrinter}
		orchestrator := pipeline.NewOrchestrator(disc, grouper, iterPruner, classifier, validator, renderers)
		runErr := orchestrator.Run(ctx, crawlRoots)
		if runErr != nil {
			fmt.Fprintf(os.Stderr, "❌ Error in %s: %v\n", projectRoot, runErr)
			hasErrors = true
			continue
		}

		// Step 4: Output pass confirmation for compliant targets.
		if targetProj != nil && targetProj.Readme != nil {
			projectName := "Unknown Project"
			origs := targetProj.Readme.OriginalSegments()
			if len(origs) > 0 && origs[0].Name != "" {
				projectName = origs[0].Name
			} else {
				projectName = findProjectBasename(inputCtx.FuchsiaDir, projectRoot, inputCtx.Config)
			}

			if len(targets) == 1 {
				target := targets[0]
				if target == "" || target == "." {
					fmt.Printf("✅ Passed: %s\n", projectName)
				} else {
					fmt.Printf("✅ Passed: %s (%s)\n", projectName, target)
				}
			} else {
				fmt.Printf("✅ Passed: %s (%d files checked)\n", projectName, len(targets))
			}
		}
	}

	// Step 5: Return overall success or failure status.
	if hasErrors {
		return subcommands.ExitFailure
	}
	return subcommands.ExitSuccess
}
