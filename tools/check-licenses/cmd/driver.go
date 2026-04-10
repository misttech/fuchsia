// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"encoding/json"
	"log"
	"path/filepath"
	"time"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/directory"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/file"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/metrics"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/project"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/readme"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/result"
)

// Execute kicks-off the check-licenses runthrough.
// It is assumed that all configuration settings have been set before this is called.
func Execute() error {
	endTrack := metrics.TotalRuntime.Track()

	// Initialize all package configs.
	startInitialize := time.Now()
	log.Print("Initializing... ")
	if err := initialize(); err != nil {
		return err
	}
	log.Printf("Done. [%v]\n", time.Since(startInitialize))

	// Traverse the repository, generating a tree of Directory and File objects in memory.
	startDirectory := time.Now()
	log.Print("Discovering files and folders... ")
	_, err := directory.NewDirectory(".", nil)
	if err != nil {
		return err
	}
	log.Printf("Done. [%v]\n", time.Since(startDirectory))

	// If we plan on generating an output notice file:
	// Filter out the projects that we don't care about (absent from the build graph).
	if Config.OutputLicenseFile {
		startFilter := time.Now()
		target := Config.Target
		if target == "" {
			target = "//:default"
		}
		log.Printf("Filtering out projects that are not in the build graph for [%s]...",
			target)
		if err := project.FilterProjects(); err != nil {
			return err
		}
		log.Printf("Done. [%v]\n", time.Since(startFilter))
	} else {
		for _, p := range project.GetAllProjects() {
			project.AddFilteredProject(p)
		}
		project.RootProject, _ = project.GetProject(".")
	}

	// License analysis happens in CQ.
	// There is no need to analyze them if all we want to do is produce a NOTICE file.
	if Config.RunAnalysis {
		// Analyze the remaining projects, and keep track of all found license texts.
		startAnalyze := time.Now()
		log.Printf("Searching for license texts [%v projects]... ", len(project.GetAllFilteredProjects()))
		err = project.AnalyzeLicenses()
		if err != nil {
			return err
		}
		log.Printf("Done. [%v]\n", time.Since(startAnalyze))
	}

	// Save the resulting NOTICE file (if necessary), all config files
	// and execution metrics to the output directory.
	// Also perform checks to ensure the repository is in a good state.
	startSaveResults := time.Now()
	log.Print("Saving results... ")
	err = result.SaveResults()
	if err != nil {
		return err
	}
	log.Printf("Done. [%v]\n", time.Since(startSaveResults))

	// Done.
	endTrack() // Capture total execution time before printing summary

	// Print standard terminal metrics summary
	log.Println("\n[check-licenses] Execution Summary")
	log.Println("----------------------------------")
	log.Printf("Total Wall Time:                  %v\n", metrics.TotalRuntime.GetTotalDuration())
	log.Printf("Time spent in GN Filter:          %v\n", metrics.FilterDuration.GetTotalDuration())
	log.Printf("Wall time spent in Classifier:    %v\n", metrics.AnalyzeDuration.GetTotalDuration())
	log.Printf("Thread time spent in Classifier:  %v\n", metrics.ClassifierDuration.GetTotalDuration())
	projectsAnalyzed, err := metrics.ProjectsProcessed.GetCount("analyzed")
	if err != nil {
		return err
	}
	log.Printf("Projects Analyzed:         %d\n", projectsAnalyzed)

	rawTexts, err := metrics.LicenseDeduplication.GetCount("raw_texts")
	if err != nil {
		return err
	}
	uniqueTexts, err := metrics.LicenseDeduplication.GetCount("unique_texts")
	if err != nil {
		return err
	}
	compression := 0.0
	if rawTexts > 0 {
		compression = float64(rawTexts-uniqueTexts) / float64(rawTexts) * 100.0
	}
	log.Printf("Licenses Deduplicated:     %.1f%% compression (%d raw -> %d unique)\n", compression, rawTexts, uniqueTexts)

	var validationErrors int64 = 0
	var allowlistHits int64 = 0

	for _, check := range Config.Result.Checks {
		vErr, err := metrics.ValidationErrors.GetCount(check.Name)
		if err != nil {
			return err
		}
		validationErrors += vErr

		aHits, err := metrics.AllowlistHits.GetCount(check.Name)
		if err != nil {
			return err
		}
		allowlistHits += aHits
	}

	log.Printf("Validation Errors:         %d (%d Hidden by Allowlist)\n", validationErrors, allowlistHits)

	if Config.OutDir != "" {
		metricsExportPath := filepath.Join(Config.OutDir, "metrics.json")
		if err := metrics.Export(metricsExportPath); err != nil {
			log.Printf("Failed to export metrics to JSON: %v\n", err)
		} else {
			log.Printf("\nExported full metrics to:  %s\n", metricsExportPath)
		}
	}

	return nil
}

// Initialize each go package with their updated config files.
func initialize() error {
	if err := file.Initialize(Config.File); err != nil {
		return err
	}
	if err := readme.Initialize(); err != nil {
		return err
	}
	if err := project.Initialize(Config.Project); err != nil {
		return err
	}
	if err := directory.Initialize(Config.Directory); err != nil {
		return err
	}
	if err := result.Initialize(Config.Result); err != nil {
		return err
	}

	// Save the config file to the out directory (if defined).
	if b, err := json.MarshalIndent(Config, "", "  "); err != nil {
		return err
	} else {
		metrics.AddArtifact("cmd/_config.json", b)
	}

	return nil
}
