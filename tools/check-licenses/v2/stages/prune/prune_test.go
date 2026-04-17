// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package prune

import (
	"context"
	"path/filepath"
	"testing"
	"time"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
)

func TestPruner_Run(t *testing.T) {
	validFile := filepath.Join("/", "workspace", "src", "used.cc")
	invalidFile := filepath.Join("/", "workspace", "src", "unused.cc")

	pruner := NewPruner(map[string]bool{
		validFile: true,
	})

	inChan := make(chan pipeline.Project, 2)

	// Project 1 has a valid file (should be kept)
	inChan <- pipeline.Project{
		RootPath: filepath.Join("/", "workspace", "src", "proj1"),
		Files:    []pipeline.FileInfo{{Path: validFile}, {Path: invalidFile}},
	}

	// Project 2 has no valid files (should be dropped)
	inChan <- pipeline.Project{
		RootPath: filepath.Join("/", "workspace", "src", "proj2"),
		Files:    []pipeline.FileInfo{{Path: invalidFile}},
	}
	close(inChan)

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	outChan, err := pruner.Run(ctx, inChan)
	if err != nil {
		t.Fatalf("Failed to run pruner: %v", err)
	}

	var results []pipeline.FilteredProject
	for p := range outChan {
		results = append(results, p)
	}

	if len(results) != 1 {
		t.Fatalf("Expected exactly 1 filtered project, got %d", len(results))
	}

	expectedPath := filepath.Join("/", "workspace", "src", "proj1")
	if results[0].RootPath != expectedPath {
		t.Errorf("Expected proj1 to survive pruning, got %v", results[0].RootPath)
	}
}

func TestPruner_EmptyValidFiles(t *testing.T) {
	// If no valid files are provided, it should pass everything through
	pruner := NewPruner(nil)

	inChan := make(chan pipeline.Project, 1)
	inChan <- pipeline.Project{
		RootPath: "/test",
		Files:    []pipeline.FileInfo{{Path: "/test/file.cc"}},
	}
	close(inChan)

	ctx := context.Background()
	outChan, _ := pruner.Run(ctx, inChan)

	count := 0
	for range outChan {
		count++
	}

	if count != 1 {
		t.Errorf("Expected 1 project when ValidFiles is empty, got %d", count)
	}
}
