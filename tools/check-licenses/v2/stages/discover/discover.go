// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package discover

import (
	"context"
	"io/fs"
	"log"
	"path/filepath"
	"strings"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/metrics"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
)

// Crawler implements pipeline.Discoverer using standard Go filesystem traversal.
// It filters paths based on FuchsiaDir, SkipPaths, and SkipAnywhere arrays.
type Crawler struct {
	FuchsiaDir   string
	SkipPaths    []string
	SkipAnywhere []string
}

// NewCrawler creates a new stateless crawler.
func NewCrawler(fuchsiaDir string, skipPaths []string, skipAnywhere []string) *Crawler {
	absFuchsiaDir, err := filepath.Abs(fuchsiaDir)
	if err == nil {
		fuchsiaDir = absFuchsiaDir
	}

	return &Crawler{
		FuchsiaDir:   fuchsiaDir,
		SkipPaths:    skipPaths,
		SkipAnywhere: skipAnywhere,
	}
}

// isSkipped checks if the given absolute path matches any skip rules.
func (c *Crawler) isSkipped(absPath string) bool {
	// Skip Anywhere (e.g. .git)
	base := filepath.Base(absPath)
	for _, skip := range c.SkipAnywhere {
		if base == skip {
			return true
		}
	}

	// Skip Paths (e.g. out, prebuilt) - These are evaluated relative to FuchsiaDir
	relPath, err := filepath.Rel(c.FuchsiaDir, absPath)
	if err != nil {
		return false
	}

	// We want to skip if relPath is exactly the skip rule, OR if it starts with the skip rule + "/"
	for _, skip := range c.SkipPaths {
		if relPath == skip || strings.HasPrefix(relPath, skip+string(filepath.Separator)) {
			return true
		}
	}

	return false
}

// Run walks the given root directories and streams discovered paths into the returned channel.
func (c *Crawler) Run(ctx context.Context, rootDirs []string) (<-chan pipeline.RawPath, error) {
	out := make(chan pipeline.RawPath)

	go func() {
		defer close(out)
		defer metrics.DirectoryTraversalDuration.Track()()

		for _, root := range rootDirs {
			// Resolve absolute path to ensure consistent downstream processing
			absRoot, err := filepath.Abs(root)
			if err != nil {
				log.Printf("Failed to resolve absolute path for %q: %v\n", root, err)
				continue
			}

			err = filepath.WalkDir(absRoot, func(path string, d fs.DirEntry, err error) error {
				// Check for context cancellation
				if ctx.Err() != nil {
					return ctx.Err()
				}
				if err != nil {
					// Log and continue if a specific file/dir has permissions issues
					log.Printf("Error accessing path %q: %v\n", path, err)
					return nil
				}

				if c.isSkipped(path) {
					if d.IsDir() {
						return filepath.SkipDir
					}
					return nil
				}

				if !d.IsDir() {
					metrics.FilesProcessed.Inc("discovered")
				}

				// Emit the path
				select {
				case <-ctx.Done():
					return ctx.Err()
				case out <- pipeline.RawPath{
					Path:  path,
					IsDir: d.IsDir(),
				}:
				}
				return nil
			})

			if err != nil && err != context.Canceled && err != context.DeadlineExceeded {
				log.Printf("Error walking directory %q: %v\n", absRoot, err)
			}
		}
	}()

	return out, nil
}
