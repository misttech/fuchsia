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
		relToFile, _ := filepath.Rel(absDir, cf.Path)
		var bestMatch *Readme
		bestPrefixLength := -1

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

	isPrimaryLicenseFile := make(map[string]bool)
	primaryLicensesByReadme := make(map[*Readme]map[string]bool)
	for _, r := range readmes {
		primaryLicensesByReadme[r] = make(map[string]bool)
	}

	for _, cf := range foundLicenses {
		r := fileToReadme[cf.Path]
		if r == nil {
			continue
		}
		relToReadme, _ := filepath.Rel(absDir, cf.Path)

		isPrimary := cf.IsLicenseFile
		if !isPrimary {
			for _, lf := range r.LicenseFiles {
				if lf == relToReadme {
					isPrimary = true
					break
				}
			}
		}

		if isPrimary {
			isPrimaryLicenseFile[cf.Path] = true
			for _, m := range cf.Matches {
				if m.MatchType != "Copyright" && !strings.HasPrefix(m.MatchType, "_") {
					primaryLicensesByReadme[r][m.SPDXID] = true
				}
			}
		}
	}

	for _, r := range readmes {
		r.LicenseFiles = nil
		r.SourceFiles = nil
		r.Licenses = nil
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
			if filepath.Clean(nlf) == relToReadme || filepath.Clean(nlf) == relToFuchsia {
				isNonLicense = true
				break
			}
		}
		if isNonLicense {
			continue
		}

		lics := make(map[string]bool)
		for _, m := range cf.Matches {
			if isPrimaryLicenseFile[cf.Path] {
				lics[m.SPDXID] = true
			} else if m.MatchType != "Copyright" && !strings.HasPrefix(m.MatchType, "_") {
				lics[m.SPDXID] = true
			}
		}

		if isPrimaryLicenseFile[cf.Path] && len(lics) == 0 {
			lics["Unclassified"] = true
		}

		if !isPrimaryLicenseFile[cf.Path] {
			if len(lics) == 0 {
				continue
			}
			isSubset := true
			for l := range lics {
				if !primaryLicensesByReadme[r][l] {
					isSubset = false
					break
				}
			}
			if isSubset {
				continue
			}
		}

		if isPrimaryLicenseFile[cf.Path] {
			r.LicenseFiles = append(r.LicenseFiles, relToReadme)
			for l := range lics {
				r.Licenses = append(r.Licenses, l)
			}
		} else {
			r.SourceFiles = append(r.SourceFiles, relToReadme)
		}
	}

	for _, r := range readmes {
		r.Licenses = deduplicateAndSort(r.Licenses)
		r.LicenseFiles = deduplicateAndSort(r.LicenseFiles)
		r.SourceFiles = deduplicateAndSort(r.SourceFiles)
	}
}

func deduplicateAndSort(items []string) []string {
	seen := make(map[string]bool)
	var result []string
	for _, item := range items {
		trimmed := strings.TrimSpace(item)
		if trimmed != "" && !seen[trimmed] {
			seen[trimmed] = true
			result = append(result, trimmed)
		}
	}
	sort.Strings(result)
	return result
}
