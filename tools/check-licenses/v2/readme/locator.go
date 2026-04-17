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
	dir := filepath.Dir(absPath)
	var foundReadmePath string

	for {
		// Check physical
		possiblePath := filepath.Join(dir, "README.fuchsia")
		if _, err := os.Stat(possiblePath); err == nil {
			foundReadmePath = possiblePath
			break
		}

		// Check virtual
		relDir, err := filepath.Rel(fuchsiaDir, dir)
		if err == nil {
			if virtualPath, ok := outOfTreeReadmes[relDir]; ok {
				foundReadmePath = virtualPath
				break
			}
		}

		parent := filepath.Dir(dir)
		if parent == dir || parent == "." || parent == "/" {
			break
		}

		// Don't walk past Fuchsia root
		if parent == fuchsiaDir {
			// One last check at the root
			possibleRootPath := filepath.Join(parent, "README.fuchsia")
			if _, err := os.Stat(possibleRootPath); err == nil {
				foundReadmePath = possibleRootPath
			}
			break
		}

		dir = parent
	}

	if foundReadmePath == "" {
		return nil, "", nil
	}

	readmes, err := ParseFile(foundReadmePath)
	if err != nil {
		return nil, foundReadmePath, fmt.Errorf("failed to parse README: %w", err)
	}

	if len(readmes) == 0 {
		return nil, foundReadmePath, fmt.Errorf("README is empty")
	}

	// Match sub-projects based on longest Location prefix
	var bestMatch *Readme
	bestPrefixLength := -1

	// Path of the file relative to the README's directory
	relToFile, err := filepath.Rel(filepath.Dir(foundReadmePath), absPath)
	if err != nil {
		return nil, "", err
	}

	for _, r := range readmes {
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

	return bestMatch, foundReadmePath, nil
}
