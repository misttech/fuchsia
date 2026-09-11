// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package report

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/pipeline"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/readme"
)

// TargetComplianceVerifier checks that individual target files or sub-directories
// have their detected licenses declared in their parent README.fuchsia.
type TargetComplianceVerifier struct {
	FuchsiaDir  string
	TargetPaths []string
	Config      readme.Config
}

// NewTargetComplianceVerifier creates a new TargetComplianceVerifier.
func NewTargetComplianceVerifier(fuchsiaDir string, config readme.Config, targetPaths ...string) *TargetComplianceVerifier {
	var canonicalTargets []string
	for _, tp := range targetPaths {
		if tp == "" {
			continue
		}
		clean := filepath.Clean(tp)
		if !filepath.IsAbs(clean) && fuchsiaDir != "" {
			clean = filepath.Join(fuchsiaDir, clean)
		}
		canonicalTargets = append(canonicalTargets, clean)
	}
	return &TargetComplianceVerifier{
		FuchsiaDir:  fuchsiaDir,
		TargetPaths: canonicalTargets,
		Config:      config,
	}
}

func (v *TargetComplianceVerifier) Run(ctx context.Context, projects []*pipeline.Project, errors []pipeline.ComplianceError) error {
	if len(v.TargetPaths) == 0 {
		return nil
	}

	// 1. Check compliance and policy errors for target paths.
	// First-party code is governed by repository compliance policies (e.g. required Fuchsia
	// copyright headers) rather than in-tree README manifests. The validate stage produces
	// ComplianceErrors for policy violations; match them against the targeted files/directories.
	if len(errors) > 0 {
		var matchedIssues []string
		for _, e := range errors {
			filePath := e.FilePath
			if !filepath.IsAbs(filePath) && filePath != "" && v.FuchsiaDir != "" {
				filePath = filepath.Join(v.FuchsiaDir, filePath)
			}
			projPath := e.Project
			if !filepath.IsAbs(projPath) && projPath != "" && v.FuchsiaDir != "" {
				projPath = filepath.Join(v.FuchsiaDir, projPath)
			}

			for _, targetPath := range v.TargetPaths {
				if targetPath == "" {
					continue
				}
				info, statErr := os.Stat(targetPath)
				isDir := statErr == nil && info.IsDir()

				matches := false
				if isDir {
					if filePath != "" {
						rel, err := filepath.Rel(targetPath, filePath)
						if err == nil && !strings.HasPrefix(rel, "..") {
							matches = true
						}
					}
					if !matches && filePath == "" && projPath != "" {
						if projPath == targetPath {
							matches = true
						}
					}
				} else {
					if filePath == targetPath {
						matches = true
					}
				}

				if matches {
					matchedIssues = append(matchedIssues, e.Issue)
					break
				}
			}
		}
		if len(matchedIssues) > 0 {
			return fmt.Errorf("%s", strings.Join(matchedIssues, "\n\n"))
		}
	}

	for _, targetPath := range v.TargetPaths {
		if targetPath == "" {
			continue
		}

		absTarget := targetPath
		info, statErr := os.Stat(absTarget)
		isDir := statErr == nil && info.IsDir()

		for _, proj := range projects {
			if proj.Readme == nil || len(proj.Readme.Segments) == 0 {
				continue
			}

			declaredPrimary := make(map[string]bool)
			for _, r := range proj.Readme.OriginalSegments() {
				for _, l := range r.Licenses {
					declaredPrimary[l] = true
				}
			}

			relTargetClean, _ := filepath.Rel(proj.RootPath, absTarget)

			for _, cf := range proj.FoundLicenses() {
				relCf, _ := filepath.Rel(proj.RootPath, cf.Path)
				// For directory targets, only evaluate files that reside within the targeted sub-directory.
				if isDir {
					relToTarget, err := filepath.Rel(absTarget, cf.Path)
					if err != nil || strings.HasPrefix(relToTarget, "..") {
						continue
					}
				} else if filepath.Clean(relCf) != filepath.Clean(relTargetClean) {
					continue
				}
				for _, match := range cf.Matches {
					if match.MatchType != "Copyright" && !strings.HasPrefix(match.MatchType, "_") {
						if !declaredPrimary[match.SPDXID] {
							return fmt.Errorf("source file %s contains a license header (%s) but the project does not declare this license in README.fuchsia", relCf, match.SPDXID)
						}
					}
				}
			}

			if isDir {
				// 1st-party projects are governed by virtual READMEs and do not maintain in-tree README.fuchsia
				// manifests, so skip manifest parity checks.
				if proj.IsFirstParty() {
					continue
				}
				origs := proj.Readme.OriginalSegments()
				updated := proj.Readme.UpdatedSegments()
				if !readme.DeclarationsMatchAll(origs, updated) {
					return fmt.Errorf("License declarations in README.fuchsia are out of date")
				}
				continue
			}

			// 1st-party projects do not declare individual LicenseFiles entries in README.fuchsia.
			if proj.IsFirstParty() {
				continue
			}

			relTarget, err := filepath.Rel(proj.RootPath, absTarget)
			if err != nil {
				continue
			}

			var expectedEntry string
			for _, r := range proj.Readme.UpdatedSegments() {
				for _, lf := range r.LicenseFiles {
					if filepath.Clean(lf) == relTarget {
						expectedEntry = lf
						break
					}
				}
				if expectedEntry == "" {
					for _, gnf := range r.GeneratedNoticeFiles {
						if filepath.Clean(gnf) == relTarget {
							expectedEntry = gnf
							break
						}
					}
				}
				if expectedEntry != "" {
					break
				}
			}

			if expectedEntry == "" {
				continue
			}

			var actualEntry string
			for _, r := range proj.Readme.OriginalSegments() {
				for _, lf := range r.LicenseFiles {
					if filepath.Clean(lf) == relTarget {
						actualEntry = lf
						break
					}
				}
				if actualEntry == "" {
					for _, gnf := range r.GeneratedNoticeFiles {
						if filepath.Clean(gnf) == relTarget {
							actualEntry = gnf
							break
						}
					}
				}
				if actualEntry != "" {
					break
				}
			}

			if actualEntry == "" {
				return fmt.Errorf("file contains license texts but is NOT declared in README.fuchsia")
			}
		}
	}

	return nil
}
