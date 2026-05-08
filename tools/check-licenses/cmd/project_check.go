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
	"strings"

	"github.com/google/subcommands"

	v2config "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/config"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
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
	for _, targetFile := range f.Args() {
		fuchsiaDir, targetFile, err := ResolveAndValidatePath(c.fuchsiaDir, targetFile)
		if err != nil {
			fmt.Fprintf(os.Stderr, "Error: %v\n", err)
			hasErrors = true
			continue
		}
		absPath := filepath.Join(fuchsiaDir, targetFile)

		if err := c.checkFile(ctx, absPath, fuchsiaDir, config, classifier); err != nil {
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

func (c *ProjectCheckCommand) checkFile(ctx context.Context, absPath, fuchsiaDir string, config *v2config.MasterConfig, classifier *classify.Classifier) error {
	// Skip if it doesn't match target extensions, UNLESS it's a dedicated license file
	ext := filepath.Ext(absPath)
	if len(config.TargetExtensions) > 0 && !config.TargetExtensions[ext] && !classify.IsLicenseFilename(absPath) {
		// It's a binary or something else we don't care about
		return nil
	}

	cf, err := runClassifierOnFile(ctx, classifier, absPath)
	if err != nil {
		return err
	}

	// Does it have any non-Copyright matches?
	hasLicense := false
	var unexpectedMatches []string
	for _, match := range cf.Matches {
		if match.MatchType != "Copyright" {
			hasLicense = true
			unexpectedMatches = append(unexpectedMatches, match.SPDXID)
		}
	}

	if !hasLicense {
		// No license text found, we don't care if it's in the README or not.
		return nil
	}

	// It has a license! We need to ensure it's declared in the closest README.fuchsia.
	r, readmePath, err := readme.FindProjectReadme(absPath, fuchsiaDir, config.OutOfTreeReadmes)
	if err != nil {
		return fmt.Errorf("found license texts (%s) but failed to find/parse a parent README.fuchsia: %w", strings.Join(unexpectedMatches, ", "), err)
	}
	if r == nil {
		return fmt.Errorf("found license texts (%s) but could not find a parent README.fuchsia in the directory tree", strings.Join(unexpectedMatches, ", "))
	}

	// Check if the file is declared
	relToReadme, err := filepath.Rel(filepath.Dir(readmePath), absPath)
	if err != nil {
		return err
	}

	// Also handle the case where it might be relative to FuchsiaDir in legacy configs, but v2 uses relative to Readme.
	relToFuchsia, _ := filepath.Rel(fuchsiaDir, absPath)

	isDeclared := false
	for _, lf := range r.LicenseFiles {
		if filepath.Clean(lf.Path) == relToReadme || filepath.Clean(lf.Path) == relToFuchsia {
			isDeclared = true
			break
		}
	}
	for _, sf := range r.SourceFiles {
		if filepath.Clean(sf.Path) == relToReadme || filepath.Clean(sf.Path) == relToFuchsia {
			isDeclared = true
			break
		}
	}
	for _, nlf := range r.NonLicenseFiles {
		if filepath.Clean(nlf.Path) == relToReadme || filepath.Clean(nlf.Path) == relToFuchsia {
			isDeclared = true
			break
		}
	}

	if !isDeclared {
		return fmt.Errorf("file contains license texts (%s) but is NOT declared in %s as a 'License File', 'Source File', or 'Non-License File'", strings.Join(unexpectedMatches, ", "), readmePath)
	}

	return nil
}

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
