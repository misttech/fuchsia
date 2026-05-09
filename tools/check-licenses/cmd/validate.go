// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"context"
	"flag"
	"fmt"
	"log"
	"os"
	"time"

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

type ValidateCommand struct {
	fuchsiaDir        string
	outDir            string
	logLevel          int
	filesInReadmeOnly bool
}

func (*ValidateCommand) Name() string { return "validate" }
func (*ValidateCommand) Synopsis() string {
	return "Audits the repository for compliance and verifies README.fuchsia files are up to date."
}
func (*ValidateCommand) Usage() string {
	return `validate [options]:
  Crawls the entire repository (or only files listed in READMEs if configured),
  runs the license classifier, checks policy exceptions, and verifies that
  README.fuchsia files are up to date. Returns a non-zero exit code if any
  compliance errors are found. Use the 'fix' command to automatically update READMEs.
`
}

func (p *ValidateCommand) SetFlags(f *flag.FlagSet) {
	f.StringVar(&p.fuchsiaDir, "fuchsia_dir", os.Getenv("FUCHSIA_DIR"), "Location of the fuchsia root directory (//).")
	f.StringVar(&p.outDir, "out_dir", "/tmp/check-licenses", "Directory to write logs.")
	f.IntVar(&p.logLevel, "log_level", 2, "Log level. 0: none, 1: file, 2: stdout+file.")
	f.BoolVar(&p.filesInReadmeOnly, "files_in_readme_only", false, "Only classify files explicitly listed in README.fuchsia files (fast mode).")
}

func (p *ValidateCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	if w, err := getLogWriters(p.logLevel, p.outDir); err == nil {
		log.SetOutput(w)
	} else {
		return subcommands.ExitFailure
	}

	log.SetFlags(0)

	fuchsiaDir, _, err := ResolveAndValidatePath(p.fuchsiaDir, ".")
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		return subcommands.ExitFailure
	}

	log.Println("Starting v2 compliance validation...")
	startTime := time.Now()

	// 1. Assembly Phase
	builder := v2config.NewBuilder(fuchsiaDir)
	if err := builder.Assemble(); err != nil {
		fmt.Fprintf(os.Stderr, "Failed to assemble configuration: %v\n", err)
		return subcommands.ExitFailure
	}
	config := builder.Config

	log.Printf("Assembled configuration in %v", time.Since(startTime))

	// 2. Instantiate Stages
	discoverer := v2discover.NewCrawler(fuchsiaDir, config.SkipPaths, config.SkipAnywhere)

	grouper := v2boundary.NewGrouper(
		fuchsiaDir,
		config.BarrierPaths,
		config.OutOfTreeReadmes,
		p.filesInReadmeOnly,
	)

	// Validate checks the entire tree, so we don't prune any targets based on the build graph.
	// Passing nil to NewPruner makes it a no-op.
	pruner := v2prune.NewPruner(nil)

	classifier, err := v2classify.NewClassifier(0.8, []string{config.PatternsDir}, config.TargetExtensions)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to initialize classifier: %v\n", err)
		return subcommands.ExitFailure
	}

	validator := v2validate.NewValidator(fuchsiaDir, config.PolicyExceptions, config.AllowedLicenses, config.CopyrightExtensions)
	// Validate checks readmes but does not overwrite them, nor does it generate SPDX/NOTICE.
	reporter := v2report.NewReporter(fuchsiaDir, p.outDir, true, false, false, config.OutOfTreeReadmes, config.PolicyExceptions[v2config.PolicyCheckAllProjectsMustHaveALicense])

	orchestrator := v2pipeline.NewOrchestrator(discoverer, grouper, pruner, classifier, validator, reporter)

	if err := orchestrator.Run(ctx, []string{fuchsiaDir}); err != nil {
		fmt.Fprintf(os.Stderr, "Validation failed: %v\n", err)
		return subcommands.ExitFailure
	}

	var checkNames []string
	for k := range config.PolicyExceptions {
		checkNames = append(checkNames, k)
	}
	for k := range config.AllowedLicenses {
		checkNames = append(checkNames, "AllowedLicenses_"+k)
	}

	log.Printf("Validation completed successfully in %v\n", time.Since(startTime))

	if err := printMetricsSummary(checkNames, true, p.logLevel, p.outDir); err != nil {
		return subcommands.ExitFailure
	}

	return subcommands.ExitSuccess
}
