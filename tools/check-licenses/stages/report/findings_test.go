// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package report

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/pipeline"
)

func TestFindingsReporter(t *testing.T) {
	tempDir := t.TempDir()
	findingsFile := filepath.Join(tempDir, "findings.json")

	reporter := NewFindingsReporter(tempDir, findingsFile)

	errors := []pipeline.ComplianceError{
		{
			CheckName: "AllLicensePatternUsagesMustBeApproved",
			LicenseID: "GPL-2.0-only",
			Project:   filepath.Join(tempDir, "third_party/foo"),
			FilePath:  filepath.Join(tempDir, "third_party/foo/LICENSE"),
			StartLine: 10,
			EndLine:   25,
			Issue:     "File was not approved to use GPL-2.0-only",
		},
		{
			CheckName: "AllProjectsMustHaveAReadme",
			Project:   filepath.Join(tempDir, "third_party/bar"),
			FilePath:  filepath.Join(tempDir, "third_party/bar/README.fuchsia"),
			StartLine: 0,
			EndLine:   0,
			Issue:     "Third-party project is missing a README.fuchsia file",
		},
	}

	if err := reporter.Run(context.Background(), nil, errors); err != nil {
		t.Fatalf("FindingsReporter.Run failed: %v", err)
	}

	data, err := os.ReadFile(findingsFile)
	if err != nil {
		t.Fatalf("Failed to read findings file: %v", err)
	}

	var findings []Finding
	if err := json.Unmarshal(data, &findings); err != nil {
		t.Fatalf("Failed to parse findings JSON: %v", err)
	}

	if len(findings) != 2 {
		t.Fatalf("Expected 2 findings, got %d", len(findings))
	}

	// Finding 1: File-level (third_party/bar/README.fuchsia)
	f1 := findings[0]
	if f1.FilePath != "third_party/bar/README.fuchsia" {
		t.Errorf("Expected FilePath third_party/bar/README.fuchsia, got %s", f1.FilePath)
	}
	if f1.Line != 0 {
		t.Errorf("Expected Line 0 (omitted for file-level), got %d", f1.Line)
	}
	if f1.EndLine != 0 {
		t.Errorf("Expected EndLine 0 (omitted for file-level), got %d", f1.EndLine)
	}

	// Finding 2: Inline range (third_party/foo/LICENSE)
	f2 := findings[1]
	if f2.FilePath != "third_party/foo/LICENSE" {
		t.Errorf("Expected FilePath third_party/foo/LICENSE, got %s", f2.FilePath)
	}
	if f2.Line != 10 {
		t.Errorf("Expected Line 10, got %d", f2.Line)
	}
	if f2.EndLine != 25 {
		t.Errorf("Expected EndLine 25, got %d", f2.EndLine)
	}
}
