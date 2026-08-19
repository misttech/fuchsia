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
	FuchsiaDir string
	Config     Config
}

// NewGrouper creates a new stateless boundary grouper.
func NewGrouper(fuchsiaDir string, config Config) *Grouper {
	absFuchsiaDir, err := filepath.Abs(fuchsiaDir)
	if err == nil {
		fuchsiaDir = absFuchsiaDir
	}
	fuchsiaDir = filepath.Clean(fuchsiaDir)

	return &Grouper{
		FuchsiaDir: fuchsiaDir,
		Config:     config,
	}
}

// Run buffers the incoming paths, determines their project boundaries, and emits the grouped projects.
func (g *Grouper) Run(ctx context.Context, in <-chan pipeline.RawPath) (<-chan pipeline.Project, error) {
	out := make(chan pipeline.Project, 100)

	go func() {
		defer close(out)

		var allFiles []string
		// physicalReadmes maps an absolute directory path to its README.fuchsia, go.mod, or Cargo.toml
		physicalReadmes := make(map[string][]string)

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
			if base == "README.fuchsia" {
				dir := filepath.Dir(cleanPath)
				physicalReadmes[dir] = append(physicalReadmes[dir], cleanPath)
			} else if base == "go.mod" || base == "Cargo.toml" || base == "pubspec.yaml" {
				// Only treat package manifests as project boundaries if they reside in third_party or vendor,
				// are not nested inside a prebuilt toolchain directory, and are not example/benchmark/test sub-packages.
				relPath, err := filepath.Rel(g.FuchsiaDir, cleanPath)
				if err == nil {
					slashRel := filepath.ToSlash(relPath)
					isThirdParty := strings.HasPrefix(slashRel, "third_party/") ||
						strings.HasPrefix(slashRel, "vendor/") ||
						strings.Contains(slashRel, "/third_party/") ||
						strings.Contains(slashRel, "/vendor/")
					isPrebuilt := strings.HasPrefix(slashRel, "prebuilt/") ||
						strings.Contains(slashRel, "/prebuilt/")

					dir := filepath.Dir(slashRel)
					isSubPackage := strings.Contains(dir, "/example") ||
						strings.Contains(dir, "/benchmark") ||
						strings.Contains(dir, "/test") ||
						strings.Contains(dir, "/interop") ||
						strings.Contains(dir, "/debug_extension") ||
						strings.Contains(dir, "/doc") ||
						strings.Contains(dir, "/tools/") ||
						strings.Contains(dir, "/misc") ||
						strings.HasPrefix(slashRel, "third_party/go/src/") // e.g. go/src/vendor, go/src/cmd/vendor

					if strings.HasPrefix(slashRel, "third_party/rust_crates/mirrors/") {
						parts := strings.Split(slashRel, "/")
						if len(parts) > 5 { // third_party / rust_crates / mirrors / <repo> / Cargo.toml (5 parts)
							isSubPackage = true
						}
					}

					if isThirdParty && !isPrebuilt && !isSubPackage {
						absDir := filepath.Dir(cleanPath)
						physicalReadmes[absDir] = append(physicalReadmes[absDir], cleanPath)
					}
				}
			}
		}

		// Incorporate Virtual (Out-Of-Tree) READMEs from Config
		for logicalPath, physicalPath := range g.Config.OutOfTreeReadmes {
			absLogicalDir := filepath.Join(g.FuchsiaDir, logicalPath)
			physicalReadmes[absLogicalDir] = append(physicalReadmes[absLogicalDir], physicalPath)
		}

		// PHASE 2: Parse all READMEs to establish exact project boundaries
		// projectRoots maps a boundary directory to its parsed Readme structs (handling DEPENDENCY DIVIDER)
		projectRoots := make(map[string][]*readme.Readme)

		// First, identify all directories that have a physical or virtual README.fuchsia
		readmeDirs := make(map[string]bool)
		for dir, readmePaths := range physicalReadmes {
			for _, p := range readmePaths {
				if filepath.Base(p) == "README.fuchsia" {
					readmeDirs[dir] = true
					break
				}
			}
		}

		// Register project boundaries: README.fuchsia always registers. Package manifests (Cargo.toml,
		// pubspec.yaml, go.mod) only register if no ancestor directory already has a README.fuchsia.
		for dir, readmePaths := range physicalReadmes {
			isManifestOnly := true
			for _, p := range readmePaths {
				if filepath.Base(p) == "README.fuchsia" {
					isManifestOnly = false
					break
				}
			}

			if isManifestOnly {
				hasReadmeAncestor := false
				for parent := filepath.Dir(dir); parent != "." && parent != "/" && parent != dir; parent = filepath.Dir(parent) {
					if parent == g.FuchsiaDir {
						continue
					}
					relParent, _ := filepath.Rel(g.FuchsiaDir, parent)
					// third_party/rust_crates has a container README, but crates under it are independent projects.
					if filepath.ToSlash(relParent) == "third_party/rust_crates" {
						continue
					}
					if readmeDirs[parent] {
						hasReadmeAncestor = true
						break
					}
				}
				if hasReadmeAncestor {
					continue
				}
			}

			for _, readmePath := range readmePaths {
				rootReadmes, subReadmes, err := readme.ParseAnyMetadata(readmePath)

				if err != nil || (len(rootReadmes) == 0 && len(subReadmes) == 0) {
					// Even if parsing fails, the file exists, so it is a boundary
					if _, exists := projectRoots[dir]; !exists {
						projectRoots[dir] = nil
					}
					continue
				}

				if len(rootReadmes) > 0 {
					projectRoots[dir] = append(projectRoots[dir], rootReadmes...)
				}

				for _, subReadme := range subReadmes {
					if subReadme.Location != "" && subReadme.Location != "." {
						absSubProjectDir := filepath.Join(dir, subReadme.Location)

						// It is possible multiple sub-projects share a directory. We append them.
						projectRoots[absSubProjectDir] = append(projectRoots[absSubProjectDir], subReadme)
					}
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
				relRoot, _ := filepath.Rel(g.FuchsiaDir, root)
				proj := &pipeline.Project{
					RootPath:     root,
					Files:        []pipeline.FileInfo{},
					ManifestName: g.Config.ManifestNameFor(relRoot),
					IsPrivate:    g.Config.IsPrivateProject(relRoot),
				}
				readmePath := filepath.Join(root, "README.fuchsia")
				if rPaths, ok := physicalReadmes[root]; ok && len(rPaths) > 0 {
					readmePath = rPaths[0]
				}
				if readmes, ok := projectRoots[root]; ok && len(readmes) > 0 {
					var segs []*pipeline.ReadmeSegment
					for _, r := range readmes {
						if r != nil {
							clone := *r
							segs = append(segs, &pipeline.ReadmeSegment{
								Original: r,
								Updated:  &clone,
							})
						}
					}
					if len(segs) > 0 {
						proj.Readme = &pipeline.ReadmeFile{
							Path:     readmePath,
							Segments: segs,
						}
					}
				}
				projects[root] = proj
			}

			// Determine if this specific file needs a custom parser based on the parsed Readmes at this root
			parser := ""
			listedInReadme := false
			isNonLicense := false
			isLicenseFile := false
			if readmes, ok := projectRoots[root]; ok {
				// Check all Readme structs registered at this boundary (handles sub-projects)
				relToReadme, _ := filepath.Rel(root, file)
				relToFuchsia, _ := filepath.Rel(g.FuchsiaDir, file)

				for _, r := range readmes {
					for _, lf := range r.LicenseFiles {
						cleanLF := filepath.Clean(lf)
						if strings.HasPrefix(cleanLF, "..") || filepath.IsAbs(lf) {
							// External license files pointing outside the project root are disallowed.
							continue
						}
						if cleanLF == relToReadme || cleanLF == relToFuchsia {
							listedInReadme = true
							isLicenseFile = true
							break
						}
					}
					if listedInReadme {
						break
					}
					for _, gnf := range r.GeneratedNoticeFiles {
						if filepath.Clean(gnf) == relToReadme || filepath.Clean(gnf) == relToFuchsia {
							listedInReadme = true
							isLicenseFile = true
							break
						}
					}
					if listedInReadme {
						break
					}
					for _, sf := range r.SourceFiles {
						if filepath.Clean(sf) == relToReadme || filepath.Clean(sf) == relToFuchsia {
							listedInReadme = true
							break
						}
					}
					if listedInReadme {
						break
					}
					for _, nlf := range r.NonLicenseFiles {
						if filepath.Clean(nlf) == relToReadme || filepath.Clean(nlf) == relToFuchsia {
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

			if g.Config.FilesInReadmeOnly && !listedInReadme {
				continue
			}

			projects[root].Files = append(projects[root].Files, pipeline.FileInfo{
				Path:          file,
				LicenseParser: parser,
				IsNonLicense:  isNonLicense,
				IsLicenseFile: isLicenseFile,
			})
		}

		// PHASE 4: Emit the projects downstream in deterministic order
		var roots []string
		for root := range projects {
			roots = append(roots, root)
		}
		sort.Strings(roots)
		for _, root := range roots {
			select {
			case <-ctx.Done():
				return
			case out <- *projects[root]:
			}
		}
	}()

	return out, nil
}

// findProjectRoot walks up the directory tree from the file to find the closest
// registered project boundary (from README.fuchsia or package manifests) or barrier root.
func (g *Grouper) findProjectRoot(filePath string, projectRoots map[string][]*readme.Readme) string {
	dir := filepath.Dir(filePath)
	var barrierChild string

	for {
		// Is this directory a registered project boundary?
		if _, isBoundary := projectRoots[dir]; isBoundary {
			if dir == g.FuchsiaDir && barrierChild != "" {
				return barrierChild
			}
			return dir
		}

		parent := filepath.Dir(dir)
		if g.isBarrier(parent) && barrierChild == "" {
			barrierChild = dir
		}

		if parent == dir || parent == "." || parent == "/" {
			break
		}
		dir = parent
	}

	if barrierChild != "" {
		return barrierChild
	}

	// Fallback to the workspace root if no boundaries exist
	return g.FuchsiaDir
}

// isBarrier checks if the given absolute directory matches a top-level defined barrier path (e.g. //third_party).
func (g *Grouper) isBarrier(absDir string) bool {
	relPath, err := filepath.Rel(g.FuchsiaDir, absDir)
	if err != nil {
		return false
	}

	slashRel := filepath.ToSlash(relPath)
	return g.Config.BarrierPaths[slashRel]
}

// BelongsToProject returns true if targetPath belongs to projectRoot rather than a nested subproject.
func (g *Grouper) BelongsToProject(targetPath, projectRoot string) bool {
	if g == nil {
		return true
	}

	cleanTarget := strings.TrimPrefix(targetPath, "//")
	cleanProjRoot := strings.TrimPrefix(projectRoot, "//")

	if cleanTarget != cleanProjRoot && cleanProjRoot != "" && cleanProjRoot != "." {
		if !strings.HasPrefix(cleanTarget, cleanProjRoot+"/") {
			return false
		}
	}

	absTarget := targetPath
	if !filepath.IsAbs(absTarget) {
		absTarget = filepath.Join(g.FuchsiaDir, cleanTarget)
	}
	r, readmePath, err := readme.FindProjectReadme(absTarget, g.FuchsiaDir, g.Config.OutOfTreeReadmes)
	if err != nil || r == nil {
		return true
	}
	var pLogicalRoot string
	for logPath, physPath := range g.Config.OutOfTreeReadmes {
		if physPath == readmePath {
			pLogicalRoot = filepath.Join(g.FuchsiaDir, logPath)
			break
		}
	}
	if pLogicalRoot == "" {
		pLogicalRoot = filepath.Dir(readmePath)
	}
	if r.Location != "" {
		pLogicalRoot = filepath.Join(pLogicalRoot, r.Location)
	}
	pRelRoot, _ := filepath.Rel(g.FuchsiaDir, pLogicalRoot)
	if pRelRoot == "." {
		pRelRoot = ""
	}
	return pRelRoot == cleanProjRoot
}
