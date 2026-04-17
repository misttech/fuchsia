// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package pipeline

import (
	"context"
	"fmt"
	"testing"
	"time"
)

// --- Mock Stages for Testing ---

type mockDiscoverer struct{}

func (m *mockDiscoverer) Run(ctx context.Context, rootDirs []string) (<-chan RawPath, error) {
	out := make(chan RawPath)
	go func() {
		defer close(out)
		out <- RawPath{Path: "test.cc"}
	}()
	return out, nil
}

type mockGrouper struct{}

func (m *mockGrouper) Run(ctx context.Context, in <-chan RawPath) (<-chan Project, error) {
	out := make(chan Project)
	go func() {
		defer close(out)
		for rp := range in {
			out <- Project{RootPath: "proj", Files: []FileInfo{{Path: rp.Path}}}
		}
	}()
	return out, nil
}

type mockPruner struct{}

func (m *mockPruner) Run(ctx context.Context, in <-chan Project) (<-chan FilteredProject, error) {
	out := make(chan FilteredProject)
	go func() {
		defer close(out)
		for p := range in {
			out <- FilteredProject{Project: p}
		}
	}()
	return out, nil
}

type mockClassifier struct{}

func (m *mockClassifier) Run(ctx context.Context, in <-chan FilteredProject) (<-chan ClassifiedFile, error) {
	out := make(chan ClassifiedFile)
	go func() {
		defer close(out)
		for fp := range in {
			for _, fileInfo := range fp.Files {
				out <- ClassifiedFile{
					Path:        fileInfo.Path,
					ProjectRoot: fp.RootPath,
					Matches:     []LicenseMatch{{SPDXID: "MIT"}},
				}
			}
		}
	}()
	return out, nil
}

type mockValidator struct {
	fail bool
}

func (m *mockValidator) Run(ctx context.Context, in <-chan ClassifiedFile) (<-chan ComplianceError, error) {
	out := make(chan ComplianceError)
	go func() {
		defer close(out)
		for cf := range in {
			if m.fail {
				out <- ComplianceError{Project: cf.ProjectRoot, FilePath: cf.Path, Issue: "Fake Error"}
			}
		}
	}()
	return out, nil
}

type mockRenderer struct {
	called bool
}

func (m *mockRenderer) Run(ctx context.Context, files <-chan ClassifiedFile, errors <-chan ComplianceError) error {
	m.called = true
	var errCount int

	errChan := make(chan int)
	go func() {
		count := 0
		for range errors {
			count++
		}
		errChan <- count
	}()

	go func() {
		for range files {
			// drain
		}
	}()

	errCount = <-errChan

	if errCount > 0 {
		return fmt.Errorf("Pipeline failed with %d errors", errCount)
	}

	return nil
}

// --- Tests ---

func TestOrchestrator_RunSuccess(t *testing.T) {
	renderer := &mockRenderer{}
	orchestrator := NewOrchestrator(
		&mockDiscoverer{},
		&mockGrouper{},
		&mockPruner{},
		&mockClassifier{},
		&mockValidator{fail: false},
		renderer,
	)

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	err := orchestrator.Run(ctx, []string{"."})
	if err != nil {
		t.Fatalf("Expected successful run, got error: %v", err)
	}

	if !renderer.called {
		t.Error("Expected Renderer to be called")
	}
}

func TestOrchestrator_RunFailure(t *testing.T) {
	renderer := &mockRenderer{}
	orchestrator := NewOrchestrator(
		&mockDiscoverer{},
		&mockGrouper{},
		&mockPruner{},
		&mockClassifier{},
		&mockValidator{fail: true},
		renderer,
	)

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	err := orchestrator.Run(ctx, []string{"."})
	if err == nil {
		t.Fatalf("Expected pipeline to fail due to compliance error, got nil")
	}
}
