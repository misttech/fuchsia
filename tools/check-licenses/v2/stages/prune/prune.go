// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package prune

import (
	"context"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/metrics"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
)

// Pruner implements pipeline.Pruner. It filters out projects that contain
// no files present in the provided ValidFiles map (e.g., from a GN build graph).
type Pruner struct {
	// ValidFiles is a set of absolute file paths that are known to be part of the build.
	ValidFiles map[string]bool
}

// NewPruner creates a new stateless project pruner.
func NewPruner(validFiles map[string]bool) *Pruner {
	return &Pruner{
		ValidFiles: validFiles,
	}
}

// Run consumes a stream of Projects, checks their files against the ValidFiles map,
// and emits FilteredProjects if they contain at least one valid file.
func (p *Pruner) Run(ctx context.Context, in <-chan pipeline.Project) (<-chan pipeline.FilteredProject, error) {
	out := make(chan pipeline.FilteredProject)

	go func() {
		defer close(out)
		defer metrics.FilterDuration.Track()()

		for proj := range in {
			if ctx.Err() != nil {
				return
			}

			// If no valid files are provided, we assume we're not pruning anything (e.g., running without a build graph).
			if len(p.ValidFiles) == 0 {
				metrics.ProjectsProcessed.Inc("kept_by_gn")
				select {
				case <-ctx.Done():
					return
				case out <- pipeline.FilteredProject{Project: proj}:
				}
				continue
			}

			// Check if any file in the project is in the build graph
			keep := false
			for _, file := range proj.Files {
				if p.ValidFiles[file.Path] {
					keep = true
					break
				}
			}

			if keep {
				metrics.ProjectsProcessed.Inc("kept_by_gn")
				select {
				case <-ctx.Done():
					return
				case out <- pipeline.FilteredProject{Project: proj}:
				}
			} else {
				metrics.ProjectsProcessed.Inc("pruned_by_gn")
			}
		}
	}()

	return out, nil
}
