// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package validate

import (
	"context"
	"fmt"
	"path/filepath"
	"strings"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/metrics"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
)

// Validator implements pipeline.Validator. It acts as the Policy Engine,
// consuming ClassifiedFiles and checking them against allowed policies.
type Validator struct {
	// FuchsiaDir is the root of the workspace.
	FuchsiaDir string

	// Allowlists maps a Policy Check Name to a set of allowed project paths.
	Allowlists map[string]map[string]bool
}

// NewValidator creates a new stateless policy engine.
func NewValidator(fuchsiaDir string, allowlists map[string]map[string]bool) *Validator {
	if allowlists == nil {
		allowlists = make(map[string]map[string]bool)
	}

	// Ensure FuchsiaDir is absolute for consistent comparison
	absFuchsiaDir, err := filepath.Abs(fuchsiaDir)
	if err == nil {
		fuchsiaDir = absFuchsiaDir
	}

	return &Validator{
		FuchsiaDir: fuchsiaDir,
		Allowlists: allowlists,
	}
}

// Run cross-references ClassifiedFiles against allowed policies and emits ComplianceErrors.
func (v *Validator) Run(ctx context.Context, in <-chan pipeline.ClassifiedFile) (<-chan pipeline.ComplianceError, error) {
	out := make(chan pipeline.ComplianceError)

	go func() {
		defer close(out)
		defer metrics.ChecksDuration.Track()()

		for cf := range in {
			if ctx.Err() != nil {
				return
			}

			// We need a consistent relative path for allowlist lookups
			relPath, err := filepath.Rel(v.FuchsiaDir, cf.Path)
			if err != nil {
				// If we can't make it relative, just use the original path
				relPath = cf.Path
			}

			// Some paths might be "."
			if relPath == "." {
				relPath = ""
			}

			// 1. Check: AllLicenseTextsMustBeRecognized
			// Explicit license files (like LICENSE or NOTICE) MUST have at least one recognized license,
			// UNLESS their file path is explicitly allowlisted.
			if cf.IsLicenseFile {
				if len(cf.Matches) == 0 {
					allowed := false
					if list, ok := v.Allowlists["AllLicenseTextsMustBeRecognized"]; ok {
						if list[relPath] {
							allowed = true
							metrics.AllowlistHits.Inc("AllLicenseTextsMustBeRecognized")
						}
					}

					if !allowed {
						metrics.ValidationErrors.Inc("AllLicenseTextsMustBeRecognized")
						err := pipeline.ComplianceError{
							Project:  cf.ProjectRoot,
							FilePath: cf.Path,
							Issue:    "Unrecognized license text: no SPDX ID could be matched",
						}
						select {
						case <-ctx.Done():
							return
						case out <- err:
						}
					}
				}
			}

			// 2. Check: AllFuchsiaAuthorSourceFilesMustHaveCopyrightHeaders
			// Source code owned by Fuchsia (ProjectRoot == FuchsiaDir) MUST have a FuchsiaCopyright,
			// UNLESS their file path is explicitly allowlisted.
			isFuchsiaProject := cf.ProjectRoot == v.FuchsiaDir || cf.ProjectRoot == "." || cf.ProjectRoot == ""

			if !cf.IsLicenseFile && isFuchsiaProject {
				hasFuchsiaCopyright := false
				for _, match := range cf.Matches {
					if match.SPDXID == "FuchsiaCopyright" {
						hasFuchsiaCopyright = true
						break
					}
				}

				if !hasFuchsiaCopyright {
					allowed := false
					if list, ok := v.Allowlists["AllFuchsiaAuthorSourceFilesMustHaveCopyrightHeaders"]; ok {
						// The v1 logic sometimes uses paths relative to FuchsiaDir, sometimes just base.
						// We use the relative file path for consistency.
						if list[relPath] {
							allowed = true
							metrics.AllowlistHits.Inc("AllFuchsiaAuthorSourceFilesMustHaveCopyrightHeaders")
						}
					}

					if !allowed {
						// To avoid flagging every single JSON/XML/config file, we only enforce this on
						// common source code files that support comments.
						// The Crawler's TargetExtensions naturally handles this, but we do a sanity check.
						ext := filepath.Ext(cf.Path)
						if isSourceCodeExt(ext) {
							metrics.ValidationErrors.Inc("AllFuchsiaAuthorSourceFilesMustHaveCopyrightHeaders")
							err := pipeline.ComplianceError{
								Project:  cf.ProjectRoot,
								FilePath: cf.Path,
								Issue:    "Missing Fuchsia copyright header in first-party source file",
							}
							select {
							case <-ctx.Done():
								return
							case out <- err:
							}
						}
					}
				}
			}

			// 3. Check: AllLicensePatternUsagesMustBeApproved
			// Certain license patterns (like GPL) are restricted and must be explicitly approved for usage.
			if cf.IsLicenseFile {
				for _, match := range cf.Matches {
					needsApproval := true
					switch {
					case match.MatchType == "Copyright" || match.MatchType == "Approved" || match.MatchType == "Permissive" || match.MatchType == "Notice" || match.MatchType == "Unencumbered" || match.MatchType == "Unclassified" || strings.HasPrefix(match.MatchType, "_"):
						needsApproval = false
					}

					if needsApproval {
						allowed := false
						if list, ok := v.Allowlists["AllLicensePatternUsagesMustBeApproved"]; ok {
							if list[relPath] {
								allowed = true
								metrics.AllowlistHits.Inc("AllLicensePatternUsagesMustBeApproved")
							}
						}

						if !allowed {
							metrics.ValidationErrors.Inc("AllLicensePatternUsagesMustBeApproved")
							err := pipeline.ComplianceError{
								Project:  cf.ProjectRoot,
								FilePath: cf.Path,
								Issue:    fmt.Sprintf("File was not approved to use license pattern %s (Type: %s)", match.SPDXID, match.MatchType),
							}
							select {
							case <-ctx.Done():
								return
							case out <- err:
							}
						}
					}
				}
			}
		}
	}()

	return out, nil
}
func isSourceCodeExt(ext string) bool {
	ext = strings.ToLower(ext)
	switch ext {
	case ".cc", ".cpp", ".h", ".hh", ".hpp", ".c", ".rs", ".go", ".py", ".sh", ".gn", ".gni", ".dart", ".java", ".kt":
		return true
	}
	return false
}
