// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package pipeline

import (
	"context"
	"fmt"
	"log"
)

// Orchestrator manages the execution and channel wiring of the 6-stage compliance pipeline.
type Orchestrator struct {
	Discoverer Discoverer
	Grouper    Grouper
	Pruner     Pruner
	Classifier Classifier
	Validator  Validator
	Renderer   Renderer
}

// NewOrchestrator creates a new pipeline orchestrator with the provided stage implementations.
func NewOrchestrator(d Discoverer, g Grouper, p Pruner, c Classifier, v Validator, r Renderer) *Orchestrator {
	return &Orchestrator{
		Discoverer: d,
		Grouper:    g,
		Pruner:     p,
		Classifier: c,
		Validator:  v,
		Renderer:   r,
	}
}

// Run executes the pipeline synchronously, wiring the channels between stages
// and waiting for the final Renderer stage to complete or fail.
func (o *Orchestrator) Run(ctx context.Context, rootDirs []string) error {
	// Stage 1: Discover (Crawler)
	log.Printf("[Orchestrator] Starting Stage 1: Discovery (Crawling %v)...", rootDirs)
	rawPaths, err := o.Discoverer.Run(ctx, rootDirs)
	if err != nil {
		return fmt.Errorf("discovery stage failed: %w", err)
	}

	// Stage 2: Group (Project Boundary)
	log.Printf("[Orchestrator] Starting Stage 2: Boundary Resolution (Grouping files into projects)...")
	projects, err := o.Grouper.Run(ctx, rawPaths)
	if err != nil {
		return fmt.Errorf("grouping stage failed: %w", err)
	}

	// Stage 3: Prune (Build Graph Filter)
	log.Printf("[Orchestrator] Starting Stage 3: Pruning (Filtering projects by build graph)...")
	filteredProjects, err := o.Pruner.Run(ctx, projects)
	if err != nil {
		return fmt.Errorf("pruning stage failed: %w", err)
	}

	// Stage 4: Classify (License Identification)
	log.Printf("[Orchestrator] Starting Stage 4: Classification (Executing regex engines)...")
	classifiedFiles, err := o.Classifier.Run(ctx, filteredProjects)
	if err != nil {
		return fmt.Errorf("classification stage failed: %w", err)
	}

	// Stage 5: Validate (Policy Engine)
	log.Printf("[Orchestrator] Starting Stage 5: Validation (Checking policies)...")
	// We need to tee the classified files channel so both the Validator and Renderer can consume it.
	filesForValidator := make(chan ClassifiedFile)
	filesForRenderer := make(chan ClassifiedFile)

	go func() {
		defer close(filesForValidator)
		defer close(filesForRenderer)
		for f := range classifiedFiles {
			if ctx.Err() != nil {
				return
			}
			filesForValidator <- f
			filesForRenderer <- f
		}
	}()

	complianceErrors, err := o.Validator.Run(ctx, filesForValidator)
	if err != nil {
		return fmt.Errorf("validation stage failed to start: %w", err)
	}

	// Stage 6: Render (Report Generation)
	log.Printf("[Orchestrator] Starting Stage 6: Reporting (Generating artifacts)...")
	// The Renderer blocks until both input channels are closed (which happens when upstream completes).
	if err := o.Renderer.Run(ctx, filesForRenderer, complianceErrors); err != nil {
		return fmt.Errorf("rendering stage failed: %w", err)
	}

	log.Printf("[Orchestrator] Pipeline execution completed successfully.")
	return nil
}
