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
	v2config "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/config"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
)

// Validator implements pipeline.Validator. It acts as the Policy Engine,
// consuming ClassifiedFiles and checking them against allowed policies.
type Validator struct {
	// FuchsiaDir is the root of the workspace.
	FuchsiaDir string

	// PolicyExceptions maps a Policy Check Name (e.g., "AllProjectsMustHaveALicense") to a set of allowed project paths.
	PolicyExceptions map[string]map[string]v2config.RuleMetadata

	// AllowedLicenses maps a highly restricted SPDX ID (e.g., "GPL-2.0", "FTL") to a set of allowed project paths.
	AllowedLicenses map[string]map[string]v2config.RuleMetadata

	// CopyrightExtensions tracks extensions that require Fuchsia copyright headers.
	CopyrightExtensions map[string]bool
}

// NewValidator creates a new stateless policy engine.
func NewValidator(fuchsiaDir string, policyExceptions map[string]map[string]v2config.RuleMetadata, allowedLicenses map[string]map[string]v2config.RuleMetadata, copyrightExtensions map[string]bool) *Validator {
	if policyExceptions == nil {
		policyExceptions = make(map[string]map[string]v2config.RuleMetadata)
	}
	if allowedLicenses == nil {
		allowedLicenses = make(map[string]map[string]v2config.RuleMetadata)
	}
	if copyrightExtensions == nil {
		copyrightExtensions = make(map[string]bool)
	}

	// Ensure FuchsiaDir is absolute for consistent comparison
	absFuchsiaDir, err := filepath.Abs(fuchsiaDir)
	if err == nil {
		fuchsiaDir = absFuchsiaDir
	}

	return &Validator{
		FuchsiaDir:          fuchsiaDir,
		PolicyExceptions:    policyExceptions,
		AllowedLicenses:     allowedLicenses,
		CopyrightExtensions: copyrightExtensions,
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
					if list, ok := v.PolicyExceptions[v2config.PolicyCheckAllLicenseTextsMustBeRecognized]; ok {
						if _, ok := list[relPath]; ok {
							allowed = true
							metrics.AllowlistHits.Inc(v2config.PolicyCheckAllLicenseTextsMustBeRecognized)
						}
					}

					if !allowed {
						metrics.ValidationErrors.Inc(v2config.PolicyCheckAllLicenseTextsMustBeRecognized)
						err := pipeline.ComplianceError{
							CheckName: v2config.PolicyCheckAllLicenseTextsMustBeRecognized,
							Project:   cf.ProjectRoot,
							FilePath:  cf.Path,
							Issue:     fmt.Sprintf("Unrecognized license text: no SPDX ID could be matched. If this file is an exception, allow it by running:\n    fx check-licenses policy add -bug <BugID> AllLicenseTextsMustBeRecognized %s", relPath),
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
					if list, ok := v.PolicyExceptions[v2config.PolicyCheckAllFuchsiaAuthorSourceFilesMustHaveCopyrightHeaders]; ok {
						// The v1 logic sometimes uses paths relative to FuchsiaDir, sometimes just base.
						// We use the relative file path for consistency.
						if _, ok := list[relPath]; ok {
							allowed = true
							metrics.AllowlistHits.Inc(v2config.PolicyCheckAllFuchsiaAuthorSourceFilesMustHaveCopyrightHeaders)
						}
					}

					if !allowed {
						// TODO(https://fxbug.dev/505430724): Skip empty __init__.py files
						if filepath.Base(cf.Path) == "__init__.py" && len(cf.AnalyzedText) == 0 {
							continue
						}

						// To avoid flagging every single JSON/XML/config file, we only enforce this on
						// common source code files that support comments.
						// The Crawler's TargetExtensions naturally handles this, but we do a sanity check.
						ext := strings.ToLower(filepath.Ext(cf.Path))
						if v.CopyrightExtensions[ext] {
							metrics.ValidationErrors.Inc(v2config.PolicyCheckAllFuchsiaAuthorSourceFilesMustHaveCopyrightHeaders)
							err := pipeline.ComplianceError{
								CheckName: v2config.PolicyCheckAllFuchsiaAuthorSourceFilesMustHaveCopyrightHeaders,
								Project:   cf.ProjectRoot,
								FilePath:  cf.Path,
								Issue:     fmt.Sprintf("Missing Fuchsia copyright header in first-party source file. Fix this automatically by running:\n    fx check-licenses copyright %s", relPath),
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
						relProjRoot, _ := filepath.Rel(v.FuchsiaDir, cf.ProjectRoot)
						if list, ok := v.AllowedLicenses[match.SPDXID]; ok {
							// The python migration script grouped allowed licenses by the ProjectRoot or relative file path.
							// To be safe, we check both the specific file and its project boundary.
							if _, ok1 := list[relPath]; ok1 {
								allowed = true
							} else if _, ok2 := list[relProjRoot]; ok2 {
								allowed = true
							} else if _, ok3 := list[cf.ProjectRoot]; ok3 {
								allowed = true
							}
							if allowed {
								metrics.AllowlistHits.Inc("AllowedLicenses_" + match.SPDXID)
							}
						}

						if !allowed {
							metrics.ValidationErrors.Inc("UnapprovedLicenseUsage")
							err := pipeline.ComplianceError{
								CheckName: v2config.CheckNameAllLicensePatternUsagesMustBeApproved,
								LicenseID: match.SPDXID,
								Project:   cf.ProjectRoot,
								FilePath:  cf.Path,
								Issue:     fmt.Sprintf("File was not approved to use license pattern %s (Type: %s). To allow this project to use this license, run:\n    fx check-licenses allowlist add -bug <BugID> %s %s", match.SPDXID, match.MatchType, match.SPDXID, relProjRoot),
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
