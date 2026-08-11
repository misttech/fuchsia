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

	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/readme"
)

// TargetComplianceVerifier checks that individual target files or sub-directories
// have their detected licenses declared in their parent README.fuchsia.
type TargetComplianceVerifier struct {
	FuchsiaDir string
	TargetPath string
	Config     readme.Config
}

// NewTargetComplianceVerifier creates a new TargetComplianceVerifier.
func NewTargetComplianceVerifier(fuchsiaDir, targetPath string, config readme.Config) *TargetComplianceVerifier {
	return &TargetComplianceVerifier{
		FuchsiaDir: fuchsiaDir,
		TargetPath: targetPath,
		Config:     config,
	}
}

func (v *TargetComplianceVerifier) Run(ctx context.Context, projects []*pipeline.Project, errors []pipeline.ComplianceError) error {
	if v.TargetPath == "" {
		return nil
	}

	absTarget := v.TargetPath
	if !filepath.IsAbs(absTarget) {
		absTarget = filepath.Join(v.FuchsiaDir, v.TargetPath)
	}
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
			if !isDir && filepath.Clean(relCf) != filepath.Clean(relTargetClean) {
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
			origs := proj.Readme.OriginalSegments()
			updated := proj.Readme.UpdatedSegments()
			if !readme.DeclarationsMatchAll(origs, updated) {
				return fmt.Errorf("License declarations in README.fuchsia are out of date")
			}
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
				for _, sf := range r.SourceFiles {
					if filepath.Clean(sf) == relTarget {
						expectedEntry = sf
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
				for _, sf := range r.SourceFiles {
					if filepath.Clean(sf) == relTarget {
						actualEntry = sf
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

	return nil
}
