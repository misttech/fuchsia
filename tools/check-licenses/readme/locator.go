// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package readme

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// FindProjectReadme walks up the directory tree from absPath to find the closest
// physical README.fuchsia file or a matching virtual out-of-tree README.
// It also matches the specific file to the correct sub-project (DEPENDENCY DIVIDER)
// defined within that README.
func FindProjectReadme(absPath, fuchsiaDir string, outOfTreeReadmes map[string]string) (*Readme, string, error) {
	absPath, err := filepath.Abs(absPath)
	if err != nil {
		return nil, "", err
	}

	var dir string
	if stat, err := os.Stat(absPath); err == nil && stat.IsDir() {
		dir = absPath
	} else {
		dir = filepath.Dir(absPath)
	}

	// Special rule for Rust mirrors: boundary is always the top-level folder under mirrors/
	mirrorsPath := filepath.Join(fuchsiaDir, "third_party/rust_crates/mirrors")
	inMirrors := false
	if strings.HasPrefix(dir, mirrorsPath) {
		rel, err := filepath.Rel(mirrorsPath, dir)
		if err == nil && rel != "." {
			parts := strings.Split(rel, string(filepath.Separator))
			if len(parts) > 0 {
				dir = filepath.Join(mirrorsPath, parts[0])
				inMirrors = true
			}
		}
	}

	for {
		isBoundary, bestPath, allReadmes, err := IsProjectBoundary(dir, fuchsiaDir, outOfTreeReadmes)
		if err != nil {
			fmt.Printf("[Locator] Error checking boundary in %s: %v\n", dir, err)
		}

		if isBoundary {
			var bestMatch *Readme
			var bestReadmePath string = bestPath
			bestPrefixLength := -1

			// Path of the file relative to the README's logical directory
			logicalDir := filepath.Dir(bestPath)
			for logPath, physPath := range outOfTreeReadmes {
				if physPath == bestPath {
					logicalDir = filepath.Join(fuchsiaDir, logPath)
					break
				}
			}
			if strings.HasSuffix(filepath.ToSlash(bestPath), "assets/readmes/README.fuchsia") {
				logicalDir = fuchsiaDir
			}

			relToFile, relErr := filepath.Rel(logicalDir, absPath)
			if relErr == nil {
				for _, r := range allReadmes {
					loc := filepath.Clean(r.Location)
					if loc == "" || loc == "." {
						if bestPrefixLength < 0 {
							bestMatch = r
							bestPrefixLength = 0
						}
					} else {
						if strings.HasPrefix(relToFile, loc+"/") || relToFile == loc {
							if len(loc) > bestPrefixLength {
								bestMatch = r
								bestPrefixLength = len(loc)
							}
						}
					}
				}
			}

			if bestMatch != nil {
				return bestMatch, bestReadmePath, nil
			}

			// Fallback to the first parsed readme if no best match found!
			if len(allReadmes) > 0 {
				return allReadmes[0], bestReadmePath, nil
			}

			return nil, "", fmt.Errorf("boundary metadata failed to parse")
		}

		parent := filepath.Dir(dir)

		// Check if we've reached the repository root or the filesystem root
		if dir == fuchsiaDir || parent == dir || dir == "." || dir == "/" {
			break
		}

		if inMirrors {
			break // Don't walk up for mirrors!
		}
		dir = parent
	}

	return nil, "", nil
}

// IsProjectBoundary returns true if the given directory marks the start of a project.
// It also returns the path to the boundary file and the parsed Readme structs.
func IsProjectBoundary(dir, fuchsiaDir string, outOfTreeReadmes map[string]string) (bool, string, []*Readme, error) {
	// Special rule for Rust mirrors: boundary is always the top-level folder under mirrors/
	mirrorsPath := filepath.Join(fuchsiaDir, "third_party/rust_crates/mirrors")
	if strings.HasPrefix(dir, mirrorsPath) {
		rel, err := filepath.Rel(mirrorsPath, dir)
		if err == nil && rel != "." {
			parts := strings.Split(rel, string(filepath.Separator))
			if len(parts) > 0 {
				projectDir := filepath.Join(mirrorsPath, parts[0])
				if dir != projectDir {
					return false, "", nil, nil // Not the boundary!
				}
			}
		}
	}

	var foundReadmePaths []string

	// Check virtual
	relDir, err := filepath.Rel(fuchsiaDir, dir)
	if err == nil {
		if virtualPath, ok := outOfTreeReadmes[relDir]; ok {
			foundReadmePaths = append(foundReadmePaths, virtualPath)
		} else if relDir == "." || relDir == "" {
			if virtualPath, ok := outOfTreeReadmes["."]; ok {
				foundReadmePaths = append(foundReadmePaths, virtualPath)
			} else if virtualPath, ok := outOfTreeReadmes[""]; ok {
				foundReadmePaths = append(foundReadmePaths, virtualPath)
			}
		}
	}
	if len(foundReadmePaths) == 0 && (dir == fuchsiaDir || relDir == "." || relDir == "") {
		rootVirtual := filepath.Join(fuchsiaDir, "tools/check-licenses/assets/readmes/README.fuchsia")
		if _, err := os.Stat(rootVirtual); err == nil {
			foundReadmePaths = append(foundReadmePaths, rootVirtual)
		}
	}

	// Check physical README.fuchsia
	physReadme := filepath.Join(dir, "README.fuchsia")
	if _, err := os.Stat(physReadme); err == nil {
		foundReadmePaths = append(foundReadmePaths, physReadme)
	}

	// Always check for package manifests (e.g. go.mod, Cargo.toml, pubspec.yaml)
	// which may define sub-projects or serve as project boundaries.
	for _, name := range []string{"go.mod", "Cargo.toml", "pubspec.yaml"} {
		possiblePath := filepath.Join(dir, name)
		if _, err := os.Stat(possiblePath); err == nil {
			foundReadmePaths = append(foundReadmePaths, possiblePath)
			break
		}
	}

	var readmeCount int
	for _, p := range foundReadmePaths {
		if filepath.Base(p) == "README.fuchsia" {
			readmeCount++
		}
	}
	if readmeCount > 1 {
		// If both virtual and physical README.fuchsia exist, log a warning
		var b strings.Builder
		b.WriteString(fmt.Sprintf("⚠️ Warning, project %s has multiple READMEs:\n", relDir))
		for _, p := range foundReadmePaths {
			if filepath.Base(p) == "README.fuchsia" {
				kind := "physical"
				if strings.Contains(p, "assets") {
					kind = "virtual "
				}
				b.WriteString(fmt.Sprintf("  * %s: %s\n", kind, p))
			}
		}
		b.WriteString("Out-of-tree asset README will take priority.\n")
		fmt.Fprint(os.Stderr, b.String())
	}

	if len(foundReadmePaths) > 0 {
		var allReadmes []*Readme
		var bestPath string
		for _, p := range foundReadmePaths {
			rootReadmes, subReadmes, parseErr := ParseAnyMetadata(p)
			if parseErr == nil {
				if bestPath == "" {
					bestPath = p
				}
				allReadmes = append(allReadmes, rootReadmes...)
				allReadmes = append(allReadmes, subReadmes...)
			}
		}
		if len(allReadmes) > 0 {
			return true, bestPath, allReadmes, nil
		}
	}

	return false, "", nil, nil
}

// ResolveProjectRoot returns the governing logical project root directory for a discovered Readme and README file path.
func ResolveProjectRoot(r *Readme, readmePath, fuchsiaDir string, outOfTreeReadmes map[string]string) string {
	logicalRoot := filepath.Dir(readmePath)
	for logPath, physPath := range outOfTreeReadmes {
		if physPath == readmePath {
			logicalRoot = filepath.Join(fuchsiaDir, logPath)
			break
		}
	}
	if strings.HasSuffix(filepath.ToSlash(readmePath), "assets/readmes/README.fuchsia") {
		logicalRoot = fuchsiaDir
	}
	if r != nil && r.Location != "" && r.Location != "." {
		logicalRoot = filepath.Join(logicalRoot, r.Location)
	}
	return logicalRoot
}
