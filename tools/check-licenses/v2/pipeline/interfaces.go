// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package pipeline

import "context"

// RawPath represents the output of the Discovery Stage (Crawler).
type RawPath struct {
	Path  string
	IsDir bool
}

// Project represents the output of the Project Boundary Stage (Grouper).
type Project struct {
	RootPath string
	Files    []string
	// Metadata...
}

// FilteredProject represents the output of the Build Graph Filtering Stage (Pruner).
type FilteredProject struct {
	Project
	// Build graph specifics...
}

// ClassifiedFile represents the output of the Ingestion Stage (Classifier).
type ClassifiedFile struct {
	Path            string
	DetectedSPDXIDs []string
	RawLicenseText  []byte
}

// ComplianceError represents a violation found during the Validation Stage (Policy Engine).
type ComplianceError struct {
	Project  string
	FilePath string
	Issue    string
}

// Discoverer defines the contract for Stage 1: Filesystem Crawler.
type Discoverer interface {
	// Run emits discovered paths into the returned channel.
	Run(ctx context.Context, rootDirs []string) (<-chan RawPath, error)
}

// Grouper defines the contract for Stage 2: Project Boundary.
type Grouper interface {
	// Run consumes RawPaths and emits grouped Projects.
	Run(ctx context.Context, in <-chan RawPath) (<-chan Project, error)
}

// Pruner defines the contract for Stage 3: GN/Bazel build graph filtering.
type Pruner interface {
	// Run cross-references files in Projects against the build graph and emits FilteredProjects.
	Run(ctx context.Context, in <-chan Project) (<-chan FilteredProject, error)
}

// Classifier defines the contract for Stage 4: Worker pool & disk caching for License Classifier.
type Classifier interface {
	// Run reads, normalizes, and classifies the files within FilteredProjects.
	Run(ctx context.Context, in <-chan FilteredProject) (<-chan ClassifiedFile, error)
}

// Validator defines the contract for Stage 5: Policy Engine.
type Validator interface {
	// Run cross-references ClassifiedFiles against allowed policies and emits any errors.
	Run(ctx context.Context, in <-chan ClassifiedFile) (<-chan ComplianceError, error)
}

// Renderer defines the contract for Stage 6: Deduplication and generators.
type Renderer interface {
	// Run deduplicates and generates final artifacts from ClassifiedFiles and ComplianceErrors.
	Run(ctx context.Context, files <-chan ClassifiedFile, errors <-chan ComplianceError) error
}
