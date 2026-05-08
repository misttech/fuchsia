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
	"time"

	"github.com/google/subcommands"

	v2config "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/config"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/readme"
	v2boundary "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/boundary"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/classify"
	v2discover "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/discover"
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
	readmePath := filepath.Join(absDir, "README.fuchsia")
	var readmes []*readme.Readme
	if _, err := os.Stat(readmePath); err == nil {
		readmes, err = readme.ParseFile(readmePath)
		if err != nil {
			fmt.Fprintf(os.Stderr, "Failed to parse existing README: %v\n", err)
			return subcommands.ExitFailure
		}
	} else {
		readmes = []*readme.Readme{{
			Name:             filepath.Base(absDir),
			URL:              "https://",
			Version:          "1.0",
			SecurityCritical: "no",
		}}
	}

	discoverer := v2discover.NewCrawler(fuchsiaDir, config.SkipPaths, config.SkipAnywhere)
	rawPaths, err := discoverer.Run(ctx, []string{absDir})
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to crawl directory: %v\n", err)
		return subcommands.ExitFailure
	}

	grouper := v2boundary.NewGrouper(fuchsiaDir, config.BarrierPaths, config.OutOfTreeReadmes, false)
	projectsChan, err := grouper.Run(ctx, rawPaths)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to group projects: %v\n", err)
		return subcommands.ExitFailure
	}

	var targetProject *pipeline.Project
	for p := range projectsChan {
		if p.RootPath == absDir {
			targetProject = &p
			break
		}
	}

	if targetProject == nil {
		fmt.Fprintf(os.Stderr, "No files found for project boundary %s\n", absDir)
		return subcommands.ExitFailure
	}

	var filesToClassify []pipeline.FileInfo
	for _, f := range targetProject.Files {
		ext := filepath.Ext(f.Path)
		if len(config.TargetExtensions) > 0 && !config.TargetExtensions[ext] && !classify.IsLicenseFilename(f.Path) {
			continue
		}
		filesToClassify = append(filesToClassify, pipeline.FileInfo{
			Path:          f.Path,
			LicenseParser: f.LicenseParser,
		})
	}

	inChan := make(chan pipeline.FilteredProject, 1)
	inChan <- pipeline.FilteredProject{
		Project: pipeline.Project{
			RootPath: absDir,
			Files:    filesToClassify,
		},
	}
	close(inChan)

	outChan, err := classifier.Run(ctx, inChan)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Failed to run classifier: %v\n", err)
		return subcommands.ExitFailure
	}

	var foundLicenses []pipeline.ClassifiedFile
	for result := range outChan {
		hasLicense := false
		for _, match := range result.Matches {
			if match.MatchType != "Copyright" && !strings.HasPrefix(match.MatchType, "_") {
				hasLicense = true
				break
			}
		}
		if hasLicense {
			foundLicenses = append(foundLicenses, result)
		}
	}

	readme.UpdateWithClassifiedFiles(fuchsiaDir, absDir, readmes, foundLicenses)

	formatted := readme.Format(readmes)

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
