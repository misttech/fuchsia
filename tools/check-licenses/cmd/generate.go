// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"context"
	"fmt"
	"log"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/metrics"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/util"

	v2config "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/config"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
	v2readme "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/readme"
	v2boundary "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/boundary"
	v2classify "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/classify"
	v2discover "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/discover"
	v2prune "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/prune"
	v2report "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/report"
	v2validate "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/validate"
)

// executeV2Pipeline runs the experimental v2 compliance engine.
func (p *GenerateCommand) executeV2Pipeline(target string) error {
	log.Println("Starting v2 fast compliance pipeline...")
	startTime := time.Now()
	ctx := context.Background()

	endTrack := metrics.TotalRuntime.Track()

	// 1. Assembly Phase
	builder := v2config.NewBuilder(p.fuchsiaDir)
	if err := builder.Assemble(); err != nil {
		return fmt.Errorf("failed to assemble configuration: %w", err)
	}

	config := builder.Config
	log.Printf("Assembled configuration in %v", time.Since(startTime))

	// We still use the GN parsing logic for now to establish the build graph map
	var validFiles map[string]bool
	if p.outputLicenseFile {
		gnStart := time.Now()
		log.Printf("Generating GN project file to extract build graph... (This may take a while)")

		gn, err := util.NewGn(p.gnPath, p.buildDir)
		if err != nil {
			return err
		}
		if err := gn.GenerateProjectFile(ctx); err != nil {
			return err
		}

		log.Printf("Loading and parsing GN project.json file...")
		gen, err := util.LoadGen(p.genProjectFile)
		if err != nil {
			return err
		}

		log.Printf("Extracting transitive files from build graph...")
		validFiles, err = gen.GetTransitiveFiles(target, p.fuchsiaDir)
		if err != nil {
			return err
		}
		log.Printf("Build graph resolution complete in %v (Found %d valid files)", time.Since(gnStart), len(validFiles))
	}

	// 3. Instantiate Stages
	discoverer := v2discover.NewCrawler(p.fuchsiaDir, config.SkipPaths, config.SkipAnywhere)

	// Pass true for filesInReadmeOnly to match current behavior!
	grouper := v2boundary.NewGrouper(p.fuchsiaDir, config.BarrierPaths, config.OutOfTreeReadmes, true)

	pruner := v2prune.NewPruner(validFiles)

	patternsDir := filepath.Join(p.fuchsiaDir, "tools", "check-licenses", "assets", "patterns")
	baseClassifier, err := v2classify.NewClassifier(0.8, []string{patternsDir}, config.TargetExtensions)
	if err != nil {
		return fmt.Errorf("failed to initialize classifier: %w", err)
	}

	// Wrap in CustomClassifier!
	classifier := &CustomClassifier{
		Base:       baseClassifier,
		FuchsiaDir: p.fuchsiaDir,
	}

	validator := v2validate.NewValidator(p.fuchsiaDir, config.PolicyExceptions, config.AllowedLicenses, config.CopyrightExtensions)

	// Reporter: p.overwriteReadmeFiles is passed!
	reporter := v2report.NewReporter(p.fuchsiaDir, p.outDir, false, p.overwriteReadmeFiles, true, config.OutOfTreeReadmes, config.PolicyExceptions[v2config.PolicyCheckAllProjectsMustHaveALicense])

	orchestrator := pipeline.NewOrchestrator(discoverer, grouper, pruner, classifier, validator, reporter)

	if err := orchestrator.Run(ctx, []string{p.fuchsiaDir}); err != nil {
		return fmt.Errorf("pipeline execution failed: %w", err)
	}

	// Print errors collected by CustomClassifier
	classifier.PrintErrors()

	endTrack()

	log.Printf("v2 pipeline completed successfully in %v\n", time.Since(startTime))
	return printMetricsSummary(nil, true, p.logLevel, p.outDir)
}

func printMetricsSummary(checkNames []string, isV2 bool, logLevel int, outDir string) error {
	// Print standard terminal metrics summary
	log.Println("\n[check-licenses] Execution Summary")
	log.Println("----------------------------------")
	log.Printf("Total Wall Time:                  %v\n", metrics.TotalRuntime.GetTotalDuration())
	log.Printf("Time spent in GN Filter:          %v\n", metrics.FilterDuration.GetTotalDuration())
	log.Printf("Wall time spent in Classifier:    %v\n", metrics.AnalyzeDuration.GetTotalDuration())
	log.Printf("Thread time spent in Classifier:  %v\n", metrics.ClassifierDuration.GetTotalDuration())

	totalFiles, _ := metrics.TotalFilesProcessed.GetCount()
	licenseFiles, _ := metrics.LicenseFilesFound.GetCount()
	sourceFilesWithLic, _ := metrics.SourceFilesWithLicenses.GetCount()

	log.Printf("Total Files Processed:            %d\n", totalFiles)
	log.Printf("License Files Found:              %d\n", licenseFiles)
	log.Printf("Source Files with Licenses:       %d\n", sourceFilesWithLic)

	var projectsAnalyzed int64
	var err error
	if isV2 {
		projectsAnalyzed, err = metrics.ProjectsProcessed.GetCount("kept_by_gn")
	} else {
		projectsAnalyzed, err = metrics.ProjectsProcessed.GetCount("analyzed")
	}
	if err != nil {
		projectsAnalyzed = 0
	}
	log.Printf("Projects Analyzed:         %d\n", projectsAnalyzed)

	rawTexts, err := metrics.LicenseDeduplication.GetCount("raw_texts")
	if err != nil {
		rawTexts = 0
	}
	uniqueTexts, err := metrics.LicenseDeduplication.GetCount("unique_texts")
	if err != nil {
		uniqueTexts = 0
	}
	compression := 0.0
	if rawTexts > 0 {
		compression = float64(rawTexts-uniqueTexts) / float64(rawTexts) * 100.0
	}
	log.Printf("Licenses Deduplicated:     %.1f%% compression (%d raw -> %d unique)\n", compression, rawTexts, uniqueTexts)

	var validationErrors int64 = 0
	var allowlistHits int64 = 0

	for _, name := range checkNames {
		vErr, _ := metrics.ValidationErrors.GetCount(name)
		validationErrors += vErr

		aHits, _ := metrics.AllowlistHits.GetCount(name)
		allowlistHits += aHits
	}

	log.Printf("Validation Errors:         %d (%d Hidden by Allowlist)\n", validationErrors, allowlistHits)

	if outDir != "" {
		metricsExportPath := filepath.Join(outDir, "metrics.json")
		if err := metrics.Export(metricsExportPath); err != nil {
			log.Printf("Failed to export metrics to JSON: %v\n", err)
		} else {
			log.Printf("\nExported full metrics to:  %s\n", metricsExportPath)
		}
	}

	return nil
}

type CustomClassifier struct {
	Base       *v2classify.Classifier
	FuchsiaDir string
	Errors     []string
	mu         sync.Mutex
}

func (c *CustomClassifier) Run(ctx context.Context, in <-chan pipeline.FilteredProject) (<-chan pipeline.ClassifiedFile, error) {
	out := make(chan pipeline.ClassifiedFile)
	go func() {
		defer close(out)
		for proj := range in {
			// Read README.fuchsia to distinguish files
			readmePath := filepath.Join(proj.RootPath, "README.fuchsia")
			var readmes []*v2readme.Readme
			var err error
			if _, err := os.Stat(readmePath); err == nil {
				readmes, err = v2readme.ParseFile(readmePath)
			}

			licenseFiles := make(map[string]bool)
			sourceFiles := make(map[string]bool)
			if err == nil {
				for _, r := range readmes {
					for _, lf := range r.LicenseFiles {
						licenseFiles[filepath.Join(proj.RootPath, lf)] = true
					}
					for _, sf := range r.SourceFiles {
						sourceFiles[filepath.Join(proj.RootPath, sf)] = true
					}
				}
			}

			for _, f := range proj.Files {
				if licenseFiles[f.Path] {
					// Dedicated License File: copy verbatim!
					data, err := os.ReadFile(f.Path)
					if err != nil {
						log.Printf("Error reading license file %s: %v", f.Path, err)
						continue
					}
					// Find license type from README if possible
					licenseType := ""
					if err == nil {
						for _, r := range readmes {
							for _, lf := range r.LicenseFiles {
								if filepath.Join(proj.RootPath, lf) == f.Path {
									licenseType = strings.Join(r.Licenses, ", ")
									break
								}
							}
						}
					}

					matches := []pipeline.LicenseMatch{}
					if licenseType != "" {
						spdxIDs := strings.Split(licenseType, ",")
						for _, id := range spdxIDs {
							id = strings.TrimSpace(id)
							if id != "" {
								matches = append(matches, pipeline.LicenseMatch{
									SPDXID: id,
									Text:   data,
								})
							}
						}
					} else {
						// Fallback if no type in README
						matches = append(matches, pipeline.LicenseMatch{
							SPDXID: "Unknown",
							Text:   data,
						})
					}

					out <- pipeline.ClassifiedFile{
						Path:          f.Path,
						ProjectRoot:   proj.RootPath,
						IsLicenseFile: true,
						AnalyzedText:  data,
						Matches:       matches,
					}
				} else if sourceFiles[f.Path] {
					// Source File: must classify!
					isLicenseFile := v2classify.IsLicenseFilename(f.Path)
					// Find license type from README if possible
					licenseType := ""
					if err == nil {
						for _, r := range readmes {
							for _, sf := range r.SourceFiles {
								if filepath.Join(proj.RootPath, sf) == f.Path {
									licenseType = strings.Join(r.Licenses, ", ")
									break
								}
							}
						}
					}

					classified, err := c.Base.ClassifyFile(f.Path, proj.RootPath, isLicenseFile, licenseType)
					if err != nil {
						log.Printf("Error classifying source file %s: %v", f.Path, err)
						continue
					}

					if len(classified.Matches) == 0 {
						c.mu.Lock()
						c.Errors = append(c.Errors, fmt.Sprintf("❌ Error: Classifier could not detect a license in Source File: %s", f.Path))
						c.mu.Unlock()
						continue
					}
					out <- *classified
				} else {
					// Not in README, fallback to standard classification
					isLicenseFile := v2classify.IsLicenseFilename(f.Path)
					classified, err := c.Base.ClassifyFile(f.Path, proj.RootPath, isLicenseFile, "")
					if err == nil {
						out <- *classified
					}
				}
			}
		}
	}()
	return out, nil
}

func (c *CustomClassifier) PrintErrors() {
	c.mu.Lock()
	defer c.mu.Unlock()
	if len(c.Errors) > 0 {
		fmt.Fprintln(os.Stderr, "\n[CustomClassifier] Errors:")
		for _, err := range c.Errors {
			fmt.Fprintln(os.Stderr, err)
		}
	}
}
