// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"encoding/json"
	"log"
	"time"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/directory"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/file"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/project"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/readme"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/result"
)

// Execute kicks-off the check-licenses runthrough.
// It is assumed that all configuration settings have been set before this is called.
func Execute() error {
	start := time.Now()

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
	var s string
	s, err = result.SaveResults(Config, Metrics)
	if err != nil {
		return err
	}
	log.Printf("Done. [%v]\n", time.Since(startSaveResults))

	// Done.
	log.Printf("\nTotal runtime: %v\n============\n", time.Since(start))
	log.Println(s)
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
		plusFile("_config.json", b)
	}

	return nil
}
