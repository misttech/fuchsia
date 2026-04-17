// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package readme

import (
	"path/filepath"
	"sort"
	"strings"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
)

// UpdateWithClassifiedFiles updates a slice of Readmes in-place with the given classified files.
// It maps each classified file to the correct sub-project (based on Location) and populates the LicenseFiles arrays.
// Files that match any NonLicenseFile entries are ignored.
func UpdateWithClassifiedFiles(fuchsiaDir, absDir string, readmes []*Readme, foundLicenses []pipeline.ClassifiedFile) {
	fileToReadme := make(map[string]*Readme)
	for _, cf := range foundLicenses {
		var bestMatch *Readme
		bestPrefixLength := -1
		relToFile, _ := filepath.Rel(absDir, cf.Path)

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
		if bestMatch != nil {
			fileToReadme[cf.Path] = bestMatch
		}
	}

	for _, r := range readmes {
		r.LicenseFiles = nil
		r.LicenseFile = ""
	}

	for _, cf := range foundLicenses {
		r := fileToReadme[cf.Path]
		if r == nil {
			continue
		}

		relToReadme, _ := filepath.Rel(absDir, cf.Path)
		relToFuchsia, _ := filepath.Rel(fuchsiaDir, cf.Path)
		isNonLicense := false
		for _, nlf := range r.NonLicenseFiles {
			if filepath.Clean(nlf.Path) == relToReadme || filepath.Clean(nlf.Path) == relToFuchsia {
				isNonLicense = true
				break
			}
		}
		if isNonLicense {
			continue
		}

		lics := make(map[string]bool)
		for _, m := range cf.Matches {
			if m.MatchType != "Copyright" && !strings.HasPrefix(m.MatchType, "_") {
				lics[m.SPDXID] = true
			}
		}
		var licNames []string
		for l := range lics {
			licNames = append(licNames, l)
		}
		sort.Strings(licNames)

		r.LicenseFiles = append(r.LicenseFiles, LicenseEntry{
			Path:        relToReadme,
			License:     strings.Join(licNames, ", "),
			LicenseType: "Single License",
		})
	}

	for _, r := range readmes {
		sort.Slice(r.LicenseFiles, func(i, j int) bool {
			return r.LicenseFiles[i].Path < r.LicenseFiles[j].Path
		})
	}
}
