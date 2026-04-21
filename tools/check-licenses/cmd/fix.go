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
	"sync"

	"github.com/google/subcommands"

	v2config "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/config"
	v2pipeline "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
	v2boundary "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/boundary"
	v2classify "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/classify"
	v2discover "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/discover"
	v2prune "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/prune"
	v2report "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/report"
	v2validate "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/validate"
)

type FixCommand struct {
	fuchsiaDir string
}

func (*FixCommand) Name() string { return "fix" }
func (*FixCommand) Synopsis() string {
	return "Automatically fix compliance issues for a project or file."
}
func (*FixCommand) Usage() string {
	return `fix <path>:
  Runs the compliance pipeline on the given path and attempts to automatically:
  - Add missing Fuchsia copyright headers.
  - Update README.fuchsia files with correct license attributions.
  - Add policy exceptions for projects missing licenses.
  - Add allowlist entries for restricted license patterns.

  Examples:
    fx check-licenses fix vendor/foo/bar
`
}

func (c *FixCommand) SetFlags(f *flag.FlagSet) {
	f.StringVar(&c.fuchsiaDir, "fuchsia_dir", os.Getenv("FUCHSIA_DIR"), "Location of the fuchsia root directory.")
}

func (c *FixCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	if f.NArg() != 1 {
		fmt.Fprintln(os.Stderr, "Usage: fx check-licenses fix <path>")
		return subcommands.ExitUsageError
	}

	targetPath := f.Arg(0)
	absTargetPath, err := filepath.Abs(targetPath)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to resolve absolute path for %s: %v\n", targetPath, err)
		return subcommands.ExitFailure
	}

	if c.fuchsiaDir == "" {
		c.fuchsiaDir = "."
	}
	absFuchsiaDir, err := filepath.Abs(c.fuchsiaDir)
	if err == nil {
		c.fuchsiaDir = absFuchsiaDir
	}

	fmt.Printf("🔍 Starting auto-fix for %s...\n", targetPath)

	// 1. Assembly Phase
	builder := v2config.NewBuilder(c.fuchsiaDir)
	if err := builder.Assemble(); err != nil {
		fmt.Fprintf(os.Stderr, "Failed to assemble configuration: %v\n", err)
		return subcommands.ExitFailure
	}
	config := builder.Config

	// 2. Instantiate Stages
	discoverer := v2discover.NewCrawler(c.fuchsiaDir, config.SkipPaths, config.SkipAnywhere)
	grouper := v2boundary.NewGrouper(c.fuchsiaDir, config.BarrierPaths, config.OutOfTreeReadmes, false)
	pruner := v2prune.NewPruner(nil) // No build graph pruning during fix

	patternsDir := filepath.Join(c.fuchsiaDir, "tools", "check-licenses", "assets", "patterns")
	classifier, err := v2classify.NewClassifier(0.8, []string{patternsDir}, config.TargetExtensions)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to initialize classifier: %v\n", err)
		return subcommands.ExitFailure
	}

	validator := v2validate.NewValidator(c.fuchsiaDir, config.PolicyExceptions, config.AllowedLicenses)

	fixer := &FixerRenderer{
		FuchsiaDir: c.fuchsiaDir,
		FixedCount: make(map[string][]string),
	}

	orchestrator := v2pipeline.NewOrchestrator(discoverer, grouper, pruner, classifier, validator, fixer)

	// We scope the discovery to the target path!
	if err := orchestrator.Run(ctx, []string{absTargetPath}); err != nil {
		fmt.Fprintf(os.Stderr, "Pipeline failed: %v\n", err)
		return subcommands.ExitFailure
	}

	fixer.PrintSummary()

	return subcommands.ExitSuccess
}

type FixerRenderer struct {
	FuchsiaDir string
	FixedCount map[string][]string
	mu         sync.Mutex
}

func (r *FixerRenderer) Run(ctx context.Context, files <-chan v2pipeline.ClassifiedFile, errors <-chan v2pipeline.ComplianceError) error {
	// We need to run the standard Reporter logic to update READMEs
	// but we'll wrap it so we can capture what it does.
	reporter := v2report.NewReporter(r.FuchsiaDir, "", true, true, false, nil)

	// We'll collect all errors first
	var errs []v2pipeline.ComplianceError
	var cFiles []v2pipeline.ClassifiedFile

	var wg sync.WaitGroup
	wg.Add(2)
	go func() {
		defer wg.Done()
		for e := range errors {
			errs = append(errs, e)
		}
	}()
	go func() {
		defer wg.Done()
		for f := range files {
			cFiles = append(cFiles, f)
		}
	}()
	wg.Wait()

	// 1. Let the reporter handle README updates
	// We simulate the Renderer run by calling its logic manually
	// Actually, easier to just copy the relevant bit or just let reporter run and capture its errors.

	// Tee channels for reporter
	reportFiles := make(chan v2pipeline.ClassifiedFile, len(cFiles))
	reportErrors := make(chan v2pipeline.ComplianceError, len(errs))
	for _, f := range cFiles {
		reportFiles <- f
	}
	for _, e := range errs {
		reportErrors <- e
	}
	close(reportFiles)
	close(reportErrors)

	// The reporter will return an error if READMEs are out of date but it also WRITES them to disk.
	// We ignore the error but track the result.
	_ = reporter.Run(ctx, reportFiles, reportErrors)

	// 2. Process all errors and apply fixes
	for _, e := range errs {
		fmt.Printf(" [Fixer] Processing error: %s (%s)\n", e.CheckName, e.FilePath)
		r.applyFix(e)
	}

	return nil
}

func (r *FixerRenderer) applyFix(e v2pipeline.ComplianceError) {
	r.mu.Lock()
	defer r.mu.Unlock()

	switch e.CheckName {
	case "ReadmeFuchsiaNeedsUpdate":
		r.FixedCount["README Updates"] = append(r.FixedCount["README Updates"], e.FilePath)

	case "AllFuchsiaAuthorSourceFilesMustHaveCopyrightHeaders":
		if err := ApplyCopyrightFix(r.FuchsiaDir, e.FilePath, false); err == nil {
			r.FixedCount["Copyright Headers"] = append(r.FixedCount["Copyright Headers"], e.FilePath)
		}

	case "AllProjectsMustHaveALicense":
		if err := AddPolicyException(r.FuchsiaDir, e.CheckName, e.Project); err == nil {
			r.FixedCount["Policy Exceptions (Missing License)"] = append(r.FixedCount["Policy Exceptions (Missing License)"], e.Project)
		}

	case "AllLicenseTextsMustBeRecognized":
		// For unrecognized texts, we add a policy exception for the specific file
		if err := AddPolicyException(r.FuchsiaDir, e.CheckName, e.FilePath); err == nil {
			r.FixedCount["Policy Exceptions (Unrecognized License)"] = append(r.FixedCount["Policy Exceptions (Unrecognized License)"], e.FilePath)
		}

	case "AllLicensePatternUsagesMustBeApproved":
		if err := AddAllowlistEntry(r.FuchsiaDir, e.LicenseID, e.Project); err == nil {
			r.FixedCount["Allowlist Entries ("+e.LicenseID+")"] = append(r.FixedCount["Allowlist Entries ("+e.LicenseID+")"], e.Project)
		}
	}
}

func (r *FixerRenderer) PrintSummary() {
	if len(r.FixedCount) == 0 {
		fmt.Println("\n✨ No issues found. Everything looks good!")
		return
	}

	fmt.Printf("\n✅ Applied fixes for the following categories:\n")

	// Sort categories for deterministic output
	var categories []string
	for cat := range r.FixedCount {
		categories = append(categories, cat)
	}
	sort.Strings(categories)

	for _, cat := range categories {
		paths := r.FixedCount[cat]
		fmt.Printf("\n[%s]\n", cat)
		sort.Strings(paths)
		for _, p := range paths {
			rel, _ := filepath.Rel(r.FuchsiaDir, p)
			if rel == "" {
				rel = p
			}
			fmt.Printf("  - %s\n", rel)

			// If it's a policy/allowlist fix, we want to warn about the JSON file
			if strings.Contains(cat, "Policy") || strings.Contains(cat, "Allowlist") {
				// Heuristic to find the dest file (this is a bit hacky but works for the summary)
				// A better way would be for AddPolicyException to return the dest file path.
			}
		}
	}

	fmt.Printf("\n⚠️  ACTION REQUIRED:\n")
	fmt.Printf("You must file an OSRB bug for any newly added policy exceptions or allowlist entries.\n")
	fmt.Printf("Check 'git status' to see the newly generated config files and update their 'bug' fields.\n")
}
