// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package report

import (
	"context"
	"crypto/sha256"
	"encoding/json"
	"fmt"
	v2config "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/config"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/metrics"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/readme"
)

// Reporter implements pipeline.Renderer. It acts as the final stage of the pipeline,
// consuming all ClassifiedFiles and ComplianceErrors. It fails the pipeline if any
// errors are encountered, otherwise it deduplicates licenses and generates the final
// artifacts (NOTICE.txt, SPDX.json).
type Reporter struct {
	FuchsiaDir               string
	OutDir                   string
	VerifyReadmes            bool
	WriteReadmes             bool
	GenerateArtifacts        bool
	OutOfTreeReadmes         map[string]string
	MissingLicenseExceptions map[string]v2config.RuleMetadata
}

// NewReporter creates a new stateful Reporter that writes to the given outDir.
func NewReporter(fuchsiaDir, outDir string, verifyReadmes, writeReadmes, generateArtifacts bool, outOfTreeReadmes map[string]string, missingLicenseExceptions map[string]v2config.RuleMetadata) *Reporter {
	return &Reporter{
		FuchsiaDir:               fuchsiaDir,
		OutDir:                   outDir,
		VerifyReadmes:            verifyReadmes,
		WriteReadmes:             writeReadmes,
		GenerateArtifacts:        generateArtifacts,
		OutOfTreeReadmes:         outOfTreeReadmes,
		MissingLicenseExceptions: missingLicenseExceptions,
	}
}

// Run deduplicates and generates final artifacts from ClassifiedFiles and ComplianceErrors.
func (r *Reporter) Run(ctx context.Context, files <-chan pipeline.ClassifiedFile, errors <-chan pipeline.ComplianceError) error {
	var errs []pipeline.ComplianceError
	var cFiles []pipeline.ClassifiedFile

	var wg sync.WaitGroup
	var errsMu sync.Mutex
	var cFilesMu sync.Mutex

	// We must consume both channels concurrently so we don't block upstream stages.
	wg.Add(2)

	go func() {
		defer wg.Done()
		for e := range errors {
			errsMu.Lock()
			errs = append(errs, e)
			errsMu.Unlock()
		}
	}()

	go func() {
		defer wg.Done()
		for f := range files {
			cFilesMu.Lock()
			cFiles = append(cFiles, f)
			cFilesMu.Unlock()
		}
	}()

	wg.Wait()

	if ctx.Err() != nil {
		return ctx.Err()
	}

	// 1. Check: AllProjectsMustHaveALicense
	// Verify that every project emitted downstream had at least one valid license file.
	projectHasLicense := make(map[string]bool)
	for _, cf := range cFiles {
		if _, exists := projectHasLicense[cf.ProjectRoot]; !exists {
			projectHasLicense[cf.ProjectRoot] = false
		}
		if cf.IsLicenseFile && len(cf.Matches) > 0 {
			projectHasLicense[cf.ProjectRoot] = true
		}
	}

	for proj, hasLicense := range projectHasLicense {
		if !hasLicense {
			// Check if allowed
			relProjRoot, _ := filepath.Rel(r.FuchsiaDir, proj)
			_, allowed := r.MissingLicenseExceptions[relProjRoot]
			if allowed {
				metrics.AllowlistHits.Inc(v2config.PolicyCheckAllProjectsMustHaveALicense)
				continue
			}

			// We emit this directly into the error slice so it fails the build
			errs = append(errs, pipeline.ComplianceError{
				CheckName: v2config.PolicyCheckAllProjectsMustHaveALicense,
				Project:   proj,
				FilePath:  "",
				Issue:     fmt.Sprintf("Project has no recognized license files. Every third-party project must contain a license file. If this project is an exception, allow it by running:\n    fx check-licenses policy add -bug <BugID> AllProjectsMustHaveALicense %s", relProjRoot),
			})
		}
	}

	// 1.5 Virtual Diff: Ensure READMEs accurately reflect classified licenses
	filesByProject := make(map[string][]pipeline.ClassifiedFile)
	for _, cf := range cFiles {
		filesByProject[cf.ProjectRoot] = append(filesByProject[cf.ProjectRoot], cf)
	}

	for projRoot, projFiles := range filesByProject {
		var readmePath string

		relRoot, err := filepath.Rel(r.FuchsiaDir, projRoot)
		if err == nil {
			if overridePath, ok := r.OutOfTreeReadmes[relRoot]; ok {
				readmePath = overridePath
			}
		}

		if readmePath == "" {
			readmePath = filepath.Join(projRoot, "README.fuchsia")
		}

		readmes, err := readme.ParseFile(readmePath)
		if err != nil || len(readmes) == 0 {
			continue // Skip if no physical README.fuchsia is present
		}

		// We only pass found licenses (non-Copyright) to the updater
		var foundLicenses []pipeline.ClassifiedFile
		for _, cf := range projFiles {
			hasLicense := false
			for _, m := range cf.Matches {
				if m.MatchType != "Copyright" && !strings.HasPrefix(m.MatchType, "_") {
					hasLicense = true
					break
				}
			}
			if hasLicense {
				foundLicenses = append(foundLicenses, cf)
			}
		}

		readme.UpdateWithClassifiedFiles(r.FuchsiaDir, projRoot, readmes, foundLicenses)
		newFormatted := readme.Format(readmes)

		rawBytes, err := os.ReadFile(readmePath)
		if err != nil {
			continue
		}

		if string(rawBytes) != newFormatted {
			if r.WriteReadmes {
				if err := os.WriteFile(readmePath, []byte(newFormatted), 0644); err != nil {
					errs = append(errs, pipeline.ComplianceError{
						CheckName: v2config.CheckNameReadmeFuchsiaNeedsUpdate,
						Project:   projRoot,
						FilePath:  readmePath,
						Issue:     fmt.Sprintf("README.fuchsia is out of date, but failed to automatically update it: %v", err),
					})
				} else {
					errs = append(errs, pipeline.ComplianceError{
						CheckName: v2config.CheckNameReadmeFuchsiaNeedsUpdate,
						Project:   projRoot,
						FilePath:  readmePath,
						Issue:     "README.fuchsia was out of date and has been automatically updated. Please review and commit the changes.",
					})
				}
			} else {
				errs = append(errs, pipeline.ComplianceError{
					CheckName: v2config.CheckNameReadmeFuchsiaNeedsUpdate,
					Project:   projRoot,
					FilePath:  readmePath,
					Issue:     "README.fuchsia is out of date. Run 'fx check-licenses fix' to update it.",
				})
			}
		}
	}

	// 2. Halt on Error
	if len(errs) > 0 {
		sort.Slice(errs, func(i, j int) bool {
			if errs[i].CheckName != errs[j].CheckName {
				return errs[i].CheckName < errs[j].CheckName
			}
			if errs[i].Issue != errs[j].Issue {
				return errs[i].Issue < errs[j].Issue
			}
			if errs[i].Project != errs[j].Project {
				return errs[i].Project < errs[j].Project
			}
			return errs[i].FilePath < errs[j].FilePath
		})

		var b strings.Builder
		b.WriteString(fmt.Sprintf("Pipeline failed with %d compliance error(s):\n", len(errs)))

		lastCheckAndIssue := ""
		for _, e := range errs {
			checkAndIssue := e.Issue
			if e.CheckName != "" {
				checkAndIssue = fmt.Sprintf("[%s] %s", e.CheckName, e.Issue)
			}

			if checkAndIssue != lastCheckAndIssue {
				b.WriteString("\n" + checkAndIssue + "\n")
				lastCheckAndIssue = checkAndIssue
			}

			relProj, err := filepath.Rel(r.FuchsiaDir, e.Project)
			if err != nil || relProj == "." {
				relProj = e.Project // Fallback or if it's the root directory
			}

			relFile := ""
			if e.FilePath != "" {
				relFile, err = filepath.Rel(r.FuchsiaDir, e.FilePath)
				if err != nil {
					relFile = e.FilePath
				}
				b.WriteString(fmt.Sprintf("- %s (%s)\n", relProj, relFile))
			} else {
				b.WriteString(fmt.Sprintf("- %s\n", relProj))
			}
		}
		return fmt.Errorf(b.String())
	}

	// 2. Generate Reports
	if r.GenerateArtifacts {
		return r.generateReports(cFiles)
	}
	return nil
}

type dedupedLicense struct {
	SPDXID string
	Text   []byte
	Hash   string
}

func (r *Reporter) generateReports(files []pipeline.ClassifiedFile) error {
	defer metrics.SpdxGenerationDuration.Track()()
	// Deduplicate identical license texts
	uniqueLicenses := make(map[string]dedupedLicense)

	for _, cf := range files {
		for _, match := range cf.Matches {
			metrics.LicenseDeduplication.Inc("raw_texts")
			h := sha256.New()
			h.Write(match.Text)
			hashStr := fmt.Sprintf("%x", h.Sum(nil))

			if _, exists := uniqueLicenses[hashStr]; !exists {
				uniqueLicenses[hashStr] = dedupedLicense{
					SPDXID: match.SPDXID,
					Text:   match.Text,
					Hash:   hashStr,
				}
				metrics.LicenseDeduplication.Inc("unique_texts")
			}
		}
	}

	// If OutDir is not specified (e.g. during a dry-run or unit test), skip file I/O
	if r.OutDir == "" {
		return nil
	}

	if err := os.MkdirAll(r.OutDir, 0755); err != nil {
		return fmt.Errorf("failed to create output directory: %w", err)
	}

	// 1. Generate NOTICE.txt
	noticePath := filepath.Join(r.OutDir, "NOTICE.txt")
	noticeFile, err := os.Create(noticePath)
	if err != nil {
		return fmt.Errorf("failed to create NOTICE.txt: %w", err)
	}
	defer noticeFile.Close()

	for _, lic := range uniqueLicenses {
		noticeContent := fmt.Sprintf("=======================================================================\nSPDX ID: %s\nLicenseRef: LicenseRef-%s\n=======================================================================\n%s\n\n", lic.SPDXID, lic.Hash, string(lic.Text))
		if _, err := noticeFile.WriteString(noticeContent); err != nil {
			return err
		}
	}

	// 2. Generate minimal SPDX.json SBOM
	spdxPath := filepath.Join(r.OutDir, "SPDX.json")
	spdxFile, err := os.Create(spdxPath)
	if err != nil {
		return fmt.Errorf("failed to create SPDX.json: %w", err)
	}
	defer spdxFile.Close()

	spdxData := map[string]interface{}{
		"SPDXID":                     "SPDXRef-DOCUMENT",
		"name":                       "Fuchsia Platform",
		"dataLicense":                "CC0-1.0",
		"hasExtractedLicensingInfos": []map[string]string{},
	}

	var extracted []map[string]string
	for _, lic := range uniqueLicenses {
		extracted = append(extracted, map[string]string{
			"licenseId":     fmt.Sprintf("LicenseRef-%s", lic.Hash),
			"extractedText": string(lic.Text),
			"name":          lic.SPDXID,
		})
	}
	spdxData["hasExtractedLicensingInfos"] = extracted

	encoder := json.NewEncoder(spdxFile)
	encoder.SetIndent("", "  ")
	if err := encoder.Encode(spdxData); err != nil {
		return err
	}

	return nil
}
