// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"context"
	"fmt"
	"log"
	"time"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/metrics"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/util"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/config"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/boundary"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/classify"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/discover"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/prune"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/report"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/stages/validate"
)

// executeV2Pipeline runs the experimental v2 compliance engine.
func (p *GenerateCommand) executeV2Pipeline(ctx context.Context, target string) error {
	log.Println("Starting v2 fast compliance pipeline...")
	startTime := time.Now()

	endTrack := metrics.TotalRuntime.Track()

	// 1. Assembly Phase
	builder := config.NewBuilder(p.fuchsiaDir)
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
	discoverer := discover.NewCrawler(p.fuchsiaDir, config.Discover)

	// Pass true for filesInReadmeOnly to match current behavior!
	boundaryCfg := config.Boundary
	boundaryCfg.FilesInReadmeOnly = true
	grouper := boundary.NewGrouper(p.fuchsiaDir, boundaryCfg)

	pruner := prune.NewPruner(validFiles)

	classifier, err := classify.NewClassifier(config.Classify)
	if err != nil {
		return fmt.Errorf("failed to initialize classifier: %w", err)
	}

	validator := validate.NewValidator(p.fuchsiaDir, config.Validate)

	var renderers pipeline.MultiRenderer
	if p.overwriteReadmeFiles {
		renderers = append(renderers, report.NewReadmeWriter(p.fuchsiaDir, false))
	} else if p.verifyReadmes {
		renderers = append(renderers, report.NewReadmeVerifier(p.fuchsiaDir))
	}

	if p.outputLicenseFile {
		renderers = append(renderers,
			report.NewNoticeRenderer(p.outDir),
			report.NewSpdxRenderer(p.outDir),
		)
	}
	renderers = append(renderers,
		report.NewMetricsRenderer(p.outDir),
		report.NewConsoleErrorReporter(p.fuchsiaDir),
	)

	orchestrator := pipeline.NewOrchestrator(discoverer, grouper, pruner, classifier, validator, renderers)

	if err := orchestrator.Run(ctx, []string{p.fuchsiaDir}); err != nil {
		return fmt.Errorf("pipeline execution failed: %w", err)
	}

	endTrack()

	log.Printf("v2 pipeline completed successfully in %v\n", time.Since(startTime))
	return nil
}
