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
	v2boundary "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/boundary"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/classify"
	v2discover "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/discover"
)

type ProjectCommand struct {
	fuchsiaDir string
}

func (*ProjectCommand) Name() string     { return "project" }
func (*ProjectCommand) Synopsis() string { return "Check or update project metadata and compliance." }
func (*ProjectCommand) Usage() string {
	return `project <subcommand> [options] <args...>:
  Manage project metadata and compliance.

  Subcommands:
    check <files...>   Analyzes specific files and validates them against their parent README.fuchsia.
    info <path>        Shows the project metadata and compliance state for a given file or directory.
    list <dir>         Lists all project boundaries discovered under the directory.
    update <dir>       Automatically updates the License File declarations in a README.fuchsia.
`
}

func (c *ProjectCommand) SetFlags(f *flag.FlagSet) {
	f.StringVar(&c.fuchsiaDir, "fuchsia_dir", os.Getenv("FUCHSIA_DIR"), "Location of the fuchsia root directory (//).")
}

func (c *ProjectCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	subFlags := flag.NewFlagSet("project", flag.ContinueOnError)
	if err := subFlags.Parse(f.Args()); err != nil {
		return subcommands.ExitUsageError
	}
	subCommander := subcommands.NewCommander(subFlags, "project")
	subCommander.Register(&ProjectCheckCommand{fuchsiaDir: c.fuchsiaDir}, "")
	subCommander.Register(&ProjectInfoCommand{fuchsiaDir: c.fuchsiaDir}, "")
	subCommander.Register(&ProjectListCommand{fuchsiaDir: c.fuchsiaDir}, "")
	subCommander.Register(&ProjectUpdateCommand{fuchsiaDir: c.fuchsiaDir}, "")
	return subCommander.Execute(ctx)
}

func RunProjectPipeline(ctx context.Context, fuchsiaDir, absDir string, config *v2config.MasterConfig, classifier *classify.Classifier) ([]*readme.Readme, []*readme.Readme, string, *pipeline.Project, []pipeline.FileInfo, []pipeline.ClassifiedFile, error) {
	var originalReadmes []*readme.Readme
	var updatedReadmes []*readme.Readme
	var readmePath string

	r, foundPath, err := readme.FindProjectReadme(absDir, fuchsiaDir, config.OutOfTreeReadmes)
	if err == nil && foundPath != "" && r != nil {
		readmePath = foundPath
		originalReadmes, _ = readme.ParseFile(readmePath)
	} else {
		readmePath = filepath.Join(absDir, "README.fuchsia")
		originalReadmes = []*readme.Readme{{
			Name:             filepath.Base(absDir),
			URL:              "https://",
			Version:          "1.0",
			SecurityCritical: "no",
		}}
	}

	for _, orig := range originalReadmes {
		clone := *orig
		clone.LicenseFiles = append([]readme.LicenseEntry(nil), orig.LicenseFiles...)
		clone.SourceFiles = append([]readme.LicenseEntry(nil), orig.SourceFiles...)
		clone.NonLicenseFiles = append([]readme.NonLicenseEntry(nil), orig.NonLicenseFiles...)
		clone.UnknownFields = append([]readme.UnknownField(nil), orig.UnknownFields...)
		updatedReadmes = append(updatedReadmes, &clone)
	}

	discoverer := v2discover.NewCrawler(fuchsiaDir, config.SkipPaths, config.SkipAnywhere)
	rawPaths, err := discoverer.Run(ctx, []string{absDir})
	if err != nil {
		return nil, nil, "", nil, nil, nil, fmt.Errorf("failed to crawl directory: %w", err)
	}

	grouper := v2boundary.NewGrouper(fuchsiaDir, config.BarrierPaths, config.OutOfTreeReadmes, false)
	projectsChan, err := grouper.Run(ctx, rawPaths)
	if err != nil {
		return nil, nil, "", nil, nil, nil, fmt.Errorf("failed to group projects: %w", err)
	}

	var targetProject *pipeline.Project
	for p := range projectsChan {
		if p.RootPath == absDir {
			targetProject = &p
			break
		}
	}

	if targetProject == nil {
		return nil, nil, "", nil, nil, nil, fmt.Errorf("no files found for project boundary %s", absDir)
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

	for _, r := range originalReadmes {
		for _, lf := range r.LicenseFiles {
			if lf.LicenseReference != "" {
				absExternal := filepath.Join(absDir, lf.LicenseReference)
				if _, statErr := os.Stat(absExternal); statErr == nil {
					filesToClassify = append(filesToClassify, pipeline.FileInfo{
						Path:          absExternal,
						LicenseParser: lf.LicenseType,
						IsLicenseFile: true,
					})
				}
			}
		}
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
		return nil, nil, "", nil, nil, nil, fmt.Errorf("failed to run classifier: %w", err)
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

	readme.UpdateWithClassifiedFiles(fuchsiaDir, absDir, updatedReadmes, foundLicenses)

	return originalReadmes, updatedReadmes, readmePath, targetProject, filesToClassify, foundLicenses, nil
}

func compareLicenseEntries(a, b []readme.LicenseEntry) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i].Path != b[i].Path || a[i].License != b[i].License || a[i].LicenseFileURL != b[i].LicenseFileURL || a[i].LicenseReference != b[i].LicenseReference {
			return false
		}
		typeA := a[i].LicenseType
		if typeA == "" {
			typeA = "Single License"
		}
		typeB := b[i].LicenseType
		if typeB == "" {
			typeB = "Single License"
		}
		if typeA != typeB {
			return false
		}
	}
	return true
}

func verifyTargetCompliance(originalReadmes, updatedReadmes []*readme.Readme, absTarget, projectRoot string, isDir bool) error {
	if isDir {
		if len(originalReadmes) != len(updatedReadmes) {
			return fmt.Errorf("README.fuchsia structure is out of date")
		}
		for i := range originalReadmes {
			if !compareLicenseEntries(originalReadmes[i].LicenseFiles, updatedReadmes[i].LicenseFiles) ||
				!compareLicenseEntries(originalReadmes[i].SourceFiles, updatedReadmes[i].SourceFiles) {
				return fmt.Errorf("License declarations in README.fuchsia are out of date")
			}
		}
		return nil
	}

	relTarget, err := filepath.Rel(projectRoot, absTarget)
	if err != nil {
		return err
	}

	var expectedEntry *readme.LicenseEntry
	for _, r := range updatedReadmes {
		for _, lf := range r.LicenseFiles {
			if filepath.Clean(lf.Path) == relTarget {
				expectedEntry = &lf
				break
			}
		}
		if expectedEntry == nil {
			for _, sf := range r.SourceFiles {
				if filepath.Clean(sf.Path) == relTarget {
					expectedEntry = &sf
					break
				}
			}
		}
		if expectedEntry != nil {
			break
		}
	}

	if expectedEntry == nil {
		return nil
	}

	var actualEntry *readme.LicenseEntry
	for _, r := range originalReadmes {
		for _, lf := range r.LicenseFiles {
			if filepath.Clean(lf.Path) == relTarget {
				actualEntry = &lf
				break
			}
		}
		if actualEntry == nil {
			for _, sf := range r.SourceFiles {
				if filepath.Clean(sf.Path) == relTarget {
					actualEntry = &sf
					break
				}
			}
		}
		if actualEntry != nil {
			break
		}
	}

	if actualEntry == nil {
		return fmt.Errorf("file contains license texts (%s) but is NOT declared in README.fuchsia", expectedEntry.License)
	}

	if actualEntry.License != expectedEntry.License {
		return fmt.Errorf("declared license (%s) does not match detected license (%s)", actualEntry.License, expectedEntry.License)
	}

	typeA := actualEntry.LicenseType
	if typeA == "" {
		typeA = "Single License"
	}
	typeB := expectedEntry.LicenseType
	if typeB == "" {
		typeB = "Single License"
	}
	if typeA != typeB {
		return fmt.Errorf("declared license type (%s) does not match detected license type (%s)", typeA, typeB)
	}

	return nil
}
