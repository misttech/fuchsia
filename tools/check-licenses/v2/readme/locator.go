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
	var dir string
	if stat, err := os.Stat(absPath); err == nil && stat.IsDir() {
		dir = absPath
	} else {
		dir = filepath.Dir(absPath)
	}
	for {
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
			var bestMatch *Readme
			var bestReadmePath string
			bestPrefixLength := -1

			for _, foundPath := range foundReadmePaths {
				rootReadmes, subReadmes, parseErr := ParseAnyMetadata(foundPath)

				if parseErr != nil || (len(rootReadmes) == 0 && len(subReadmes) == 0) {
					continue
				}

				// Path of the file relative to the README's logical directory
				logicalDir := filepath.Dir(foundPath)
				for logPath, physPath := range outOfTreeReadmes {
					if physPath == foundPath {
						logicalDir = filepath.Join(fuchsiaDir, logPath)
						break
					}
				}

				relToFile, relErr := filepath.Rel(logicalDir, absPath)
				if relErr != nil {
					continue
				}

				allReadmes := append([]*Readme{}, rootReadmes...)
				allReadmes = append(allReadmes, subReadmes...)

				for _, r := range allReadmes {
					loc := filepath.Clean(r.Location)
					if loc == "" || loc == "." {
						if bestPrefixLength < 0 {
							bestMatch = r
							bestReadmePath = foundPath
							bestPrefixLength = 0
						}
					} else {
						if strings.HasPrefix(relToFile, loc+"/") || relToFile == loc {
							if len(loc) > bestPrefixLength {
								bestMatch = r
								bestReadmePath = foundPath
								bestPrefixLength = len(loc)
							}
						}
					}
				}
			}

			if bestMatch != nil {
				return bestMatch, bestReadmePath, nil
			}

			// If we got here, we found boundary files but none matched the location, or all failed to parse.
			// Return the first successfully parsed root readme as a fallback, or nil if none.
			for _, foundPath := range foundReadmePaths {
				rootReadmes, _, parseErr := ParseAnyMetadata(foundPath)
				if parseErr == nil && len(rootReadmes) > 0 {
					return rootReadmes[0], foundPath, nil
				}
			}
			return nil, "", fmt.Errorf("boundary metadata failed to parse")
		}

		parent := filepath.Dir(dir)

		// Check if we've reached the repository root or the filesystem root
		if dir == fuchsiaDir || parent == dir || dir == "." || dir == "/" {
			break
		}

		dir = parent
	}

	return nil, "", nil
}
