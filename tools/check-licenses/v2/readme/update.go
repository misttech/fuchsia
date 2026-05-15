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
	externalToFileEntry := make(map[string]string) // maps cf.Path (external) -> local Path (e.g. "LICENSE")

	for _, cf := range foundLicenses {
		relToFile, _ := filepath.Rel(absDir, cf.Path)
		var bestMatch *Readme
		bestPrefixLength := -1

		for _, r := range readmes {
			for _, lf := range r.LicenseFiles {
				if filepath.Clean(lf.LicenseReference) == relToFile {
					bestMatch = r
					externalToFileEntry[cf.Path] = lf.Path
					break
				}
			}
			if bestMatch != nil {
				break
			}

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

	existingLicenses := make(map[*Readme]map[string]LicenseEntry)
	for _, r := range readmes {
		existingLicenses[r] = make(map[string]LicenseEntry)
		if r.LicenseFile != "" {
			existingLicenses[r][r.LicenseFile] = LicenseEntry{Path: r.LicenseFile, LicenseType: "Single License"}
		}
		for _, lf := range r.LicenseFiles {
			existingLicenses[r][lf.Path] = lf
		}
		for _, sf := range r.SourceFiles {
			existingLicenses[r][sf.Path] = sf
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
		if localPath, ok := externalToFileEntry[cf.Path]; ok {
			relToReadme = localPath
		} else if existing, ok := existingLicenses[r][relToReadme]; ok && existing.LicenseReference != "" {
			// Skip local stub file! We only care about its external reference file!
			continue
		}

		isPrimary := cf.IsLicenseFile
		if localPath, ok := externalToFileEntry[cf.Path]; ok {
			isPrimary = true
			relToReadme = localPath
		} else if !isPrimary {
			if r.LicenseFile == relToReadme {
				isPrimary = true
			} else {
				for _, lf := range r.LicenseFiles {
					if lf.Path == relToReadme {
						isPrimary = true
						break
					}
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
		r.LicenseFile = ""
		r.SourceFiles = nil
	}

	for _, cf := range foundLicenses {
		r := fileToReadme[cf.Path]
		if r == nil {
			continue
		}

		relToReadme, _ := filepath.Rel(absDir, cf.Path)
		if localPath, ok := externalToFileEntry[cf.Path]; ok {
			relToReadme = localPath
		} else if existing, ok := existingLicenses[r][relToReadme]; ok && existing.LicenseReference != "" {
			continue
		}

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

		var licNames []string
		for l := range lics {
			licNames = append(licNames, l)
		}
		sort.Strings(licNames)

		licenseType := "Single License"
		licenseFileURL := ""
		licenseReference := ""
		if existing, ok := existingLicenses[r][relToReadme]; ok {
			if existing.LicenseType != "" {
				licenseType = existing.LicenseType
			}
			licenseFileURL = existing.LicenseFileURL
			licenseReference = existing.LicenseReference
		}

		entry := LicenseEntry{
			Path:             relToReadme,
			License:          strings.Join(licNames, ", "),
			LicenseType:      licenseType,
			LicenseFileURL:   licenseFileURL,
			LicenseReference: licenseReference,
		}

		if isPrimaryLicenseFile[cf.Path] {
			r.LicenseFiles = append(r.LicenseFiles, entry)
		} else {
			r.SourceFiles = append(r.SourceFiles, entry)
		}
	}

	for _, r := range readmes {
		sort.Slice(r.LicenseFiles, func(i, j int) bool {
			return r.LicenseFiles[i].Path < r.LicenseFiles[j].Path
		})
		sort.Slice(r.SourceFiles, func(i, j int) bool {
			return r.SourceFiles[i].Path < r.SourceFiles[j].Path
		})
	}
}
