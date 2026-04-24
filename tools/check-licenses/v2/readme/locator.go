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

	// Check physical
	for _, name := range []string{"README.fuchsia", "go.mod", "Cargo.toml", "pubspec.yaml"} {
		possiblePath := filepath.Join(dir, name)
		if _, err := os.Stat(possiblePath); err == nil {
			foundReadmePaths = append(foundReadmePaths, possiblePath)
		}
	}

	// Check virtual
	relDir, err := filepath.Rel(fuchsiaDir, dir)
	if err == nil {
		if virtualPath, ok := outOfTreeReadmes[relDir]; ok {
			foundReadmePaths = append(foundReadmePaths, virtualPath)
		}
	}

	if len(foundReadmePaths) > 0 {
		var allReadmes []*Readme
		var bestPath string
		for _, path := range foundReadmePaths {
			rootReadmes, subReadmes, parseErr := ParseAnyMetadata(path)
			if parseErr == nil {
				allReadmes = append(allReadmes, rootReadmes...)
				allReadmes = append(allReadmes, subReadmes...)
				if bestPath == "" {
					bestPath = path
				}
			}
		}
		if len(allReadmes) > 0 {
			return true, bestPath, allReadmes, nil
		}
	}

	return false, "", nil, nil
}
