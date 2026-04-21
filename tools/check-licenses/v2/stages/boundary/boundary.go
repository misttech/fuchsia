// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package boundary

import (
	"context"
	"path/filepath"
	"sort"
	"strings"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/readme"
)

// Grouper implements pipeline.Grouper. It consumes a stream of RawPaths,
// buffers them, identifies project boundaries (via READMEs or Barriers),
// and emits grouped Project structs.
type Grouper struct {
	FuchsiaDir        string
	BarrierPaths      []string
	OutOfTreeReadmes  map[string]string
	FilesInReadmeOnly bool
}

// NewGrouper creates a new stateless boundary grouper.
func NewGrouper(fuchsiaDir string, barrierPaths []string, outOfTreeReadmes map[string]string, filesInReadmeOnly bool) *Grouper {
	return &Grouper{
		FuchsiaDir:        fuchsiaDir,
		BarrierPaths:      barrierPaths,
		OutOfTreeReadmes:  outOfTreeReadmes,
		FilesInReadmeOnly: filesInReadmeOnly,
	}
}

// Run buffers the incoming paths, determines their project boundaries, and emits the grouped projects.
func (g *Grouper) Run(ctx context.Context, in <-chan pipeline.RawPath) (<-chan pipeline.Project, error) {
	out := make(chan pipeline.Project)

	go func() {
		defer close(out)

		var allFiles []string
		// physicalReadmes maps an absolute directory path to its README.fuchsia or Cargo.toml
		physicalReadmes := make(map[string]string)

		// PHASE 1: Consume all incoming paths
		for rp := range in {
			if ctx.Err() != nil {
				return
			}
			if rp.IsDir {
				continue
			}

			cleanPath := filepath.Clean(rp.Path)
			allFiles = append(allFiles, cleanPath)

			base := filepath.Base(cleanPath)
			if base == "README.fuchsia" || base == "Cargo.toml" {
				dir := filepath.Dir(cleanPath)
				physicalReadmes[dir] = cleanPath
			}
		}

		// Incorporate Virtual (Out-Of-Tree) READMEs from Config
		for logicalPath, physicalPath := range g.OutOfTreeReadmes {
			absLogicalDir := filepath.Join(g.FuchsiaDir, logicalPath)
			physicalReadmes[absLogicalDir] = physicalPath
		}

		// PHASE 2: Parse all READMEs to establish exact project boundaries
		// projectRoots maps a boundary directory to its parsed Readme structs (handling DEPENDENCY DIVIDER)
		projectRoots := make(map[string][]*readme.Readme)

		// First, register every directory that has a physical/virtual README or Cargo.toml as a root
		for dir, readmePath := range physicalReadmes {
			if filepath.Base(readmePath) == "Cargo.toml" {
				// We don't parse Cargo.toml yet, just register the boundary
				projectRoots[dir] = nil
				continue
			}

			// Parse README.fuchsia files
			parsedReadmes, err := readme.ParseFile(readmePath)
			if err != nil || len(parsedReadmes) == 0 {
				// Even if parsing fails, the file exists, so it is a boundary
				projectRoots[dir] = nil
				continue
			}

			// The primary project lives at the directory of the README
			projectRoots[dir] = []*readme.Readme{parsedReadmes[0]}

			// If the README defines sub-projects via DEPENDENCY DIVIDER, register them as distinct boundaries!
			for i := 1; i < len(parsedReadmes); i++ {
				subReadme := parsedReadmes[i]
				if subReadme.Location != "" {
					absSubProjectDir := filepath.Join(dir, subReadme.Location)

					// It's possible multiple sub-projects share a directory. We append them.
					projectRoots[absSubProjectDir] = append(projectRoots[absSubProjectDir], subReadme)
				}
			}
		}

		// Sort to ensure deterministic grouping
		sort.Strings(allFiles)

		// PHASE 3: Group files by their closest project root
		projects := make(map[string]*pipeline.Project)

		for _, file := range allFiles {
			if ctx.Err() != nil {
				return
			}

			root := g.findProjectRoot(file, projectRoots)

			if _, exists := projects[root]; !exists {
				projects[root] = &pipeline.Project{
					RootPath: root,
					Files:    []pipeline.FileInfo{},
				}
			}

			// Determine if this specific file needs a custom parser based on the parsed Readmes at this root
			parser := ""
			listedInReadme := false
			isNonLicense := false
			if readmes, ok := projectRoots[root]; ok {
				// Check all Readme structs registered at this boundary (handles sub-projects)
				relToReadme, _ := filepath.Rel(root, file)
				relToFuchsia, _ := filepath.Rel(g.FuchsiaDir, file)

				for _, r := range readmes {
					for _, lf := range r.LicenseFiles {
						if filepath.Clean(lf.Path) == relToReadme || filepath.Clean(lf.Path) == relToFuchsia {
							parser = lf.LicenseType
							listedInReadme = true
							break
						}
					}
					if listedInReadme {
						break
					}
					for _, nlf := range r.NonLicenseFiles {
						if filepath.Clean(nlf.Path) == relToReadme || filepath.Clean(nlf.Path) == relToFuchsia {
							listedInReadme = true
							isNonLicense = true
							break
						}
					}
					if listedInReadme {
						break
					}
				}
			}

			if g.FilesInReadmeOnly && !listedInReadme {
				continue
			}

			projects[root].Files = append(projects[root].Files, pipeline.FileInfo{
				Path:          file,
				LicenseParser: parser,
				IsNonLicense:  isNonLicense,
			})
		}

		// PHASE 4: Emit the projects downstream
		for _, proj := range projects {
			select {
			case <-ctx.Done():
				return
			case out <- *proj:
			}
		}
	}()

	return out, nil
}

// findProjectRoot walks up the directory tree from the file to find the closest
// project boundary (either a parsed README root or a Barrier).
func (g *Grouper) findProjectRoot(filePath string, projectRoots map[string][]*readme.Readme) string {
	dir := filepath.Dir(filePath)

	for {
		// Rule 1: Is this directory a registered project boundary?
		if _, isBoundary := projectRoots[dir]; isBoundary {
			return dir
		}

		// Rule 2: Is this directory an immediate child of a Barrier?
		parent := filepath.Dir(dir)
		if g.isBarrier(parent) {
			return dir
		}

		if parent == dir || parent == "." || parent == "/" {
			break
		}
		dir = parent
	}

	// Fallback to the workspace root if no boundaries exist
	return g.FuchsiaDir
}

// isBarrier checks if the given absolute directory matches a defined barrier path.
func (g *Grouper) isBarrier(absDir string) bool {
	relPath, err := filepath.Rel(g.FuchsiaDir, absDir)
	if err != nil {
		return false
	}

	for _, barrier := range g.BarrierPaths {
		if relPath == barrier || strings.HasSuffix(relPath, string(filepath.Separator)+barrier) {
			return true
		}
	}

	return false
}
