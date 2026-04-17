// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package discover

import (
	"context"
	"fmt"
	"io/fs"
	"path/filepath"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/metrics"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
)

// Crawler implements pipeline.Discoverer using standard Go filesystem traversal.
// It is a completely stateless, pure implementation of the discovery stage.
type Crawler struct{}

// NewCrawler creates a new stateless crawler.
func NewCrawler() *Crawler {
	return &Crawler{}
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
				fmt.Printf("Failed to resolve absolute path for %q: %v\n", root, err)
				continue
			}

			err = filepath.WalkDir(absRoot, func(path string, d fs.DirEntry, err error) error {
				// Check for context cancellation
				if ctx.Err() != nil {
					return ctx.Err()
				}
				if err != nil {
					// Log and continue if a specific file/dir has permissions issues
					fmt.Printf("Error accessing path %q: %v\n", path, err)
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
				fmt.Printf("Error walking directory %q: %v\n", absRoot, err)
			}
		}
	}()

	return out, nil
}
