// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package report

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
)

func TestReporter_RunSuccess(t *testing.T) {
	outDir := t.TempDir()
	reporter := NewReporter(t.TempDir(), outDir, true, true, true, nil, nil)

	filesChan := make(chan pipeline.ClassifiedFile, 3)
	errorsChan := make(chan pipeline.ComplianceError, 1) // no errors sent

	filesChan <- pipeline.ClassifiedFile{
		Path:          "third_party/foo/LICENSE",
		ProjectRoot:   "third_party/foo",
		IsLicenseFile: true,
		Matches: []pipeline.LicenseMatch{
			{SPDXID: "MIT", Text: []byte("MIT License Text A")},
		},
	}
	filesChan <- pipeline.ClassifiedFile{
		Path:          "third_party/bar/LICENSE",
		ProjectRoot:   "third_party/bar",
		IsLicenseFile: true,
		Matches: []pipeline.LicenseMatch{
			{SPDXID: "MIT", Text: []byte("MIT License Text A")}, // Duplicate text, should be deduped
			{SPDXID: "Apache-2.0", Text: []byte("Apache License Text B")},
		},
	}

	close(filesChan)
	close(errorsChan)

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	err := reporter.Run(ctx, filesChan, errorsChan)
	if err != nil {
		t.Fatalf("Expected successful run, got error: %v", err)
	}

	// Verify NOTICE.txt
	noticeBytes, err := os.ReadFile(filepath.Join(outDir, "NOTICE.txt"))
	if err != nil {
		t.Fatal("Failed to read NOTICE.txt")
	}
	notice := string(noticeBytes)

	if !strings.Contains(notice, "MIT License Text A") {
		t.Error("NOTICE.txt missing MIT text")
	}
	if !strings.Contains(notice, "Apache License Text B") {
		t.Error("NOTICE.txt missing Apache text")
	}

	// Verify SPDX.json
	spdxBytes, err := os.ReadFile(filepath.Join(outDir, "SPDX.json"))
	if err != nil {
		t.Fatal("Failed to read SPDX.json")
	}
	spdx := string(spdxBytes)

	if !strings.Contains(spdx, "SPDXRef-DOCUMENT") {
		t.Error("SPDX.json missing Document Ref")
	}
}

func TestReporter_RunFailure(t *testing.T) {
	reporter := NewReporter(t.TempDir(), "", true, false, false, nil, nil) // dry run

	filesChan := make(chan pipeline.ClassifiedFile, 1)
	errorsChan := make(chan pipeline.ComplianceError, 2)

	close(filesChan) // no files needed

	errorsChan <- pipeline.ComplianceError{
		CheckName: "UnallowedLicense",
		Project:   "third_party/bad",
		FilePath:  "third_party/bad/LICENSE",
		Issue:     "Unallowed license GPL-2.0",
	}
	close(errorsChan)

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	err := reporter.Run(ctx, filesChan, errorsChan)
	if err == nil {
		t.Fatal("Expected error due to compliance violation, got nil")
	}

	if !strings.Contains(err.Error(), "1 compliance error") {
		t.Errorf("Expected error to contain error count, got: %v", err)
	}
	if !strings.Contains(err.Error(), "Unallowed license GPL-2.0") {
		t.Errorf("Expected error to contain issue description, got: %v", err)
	}
}

func TestReporter_RunFailure_MissingLicense(t *testing.T) {
	reporter := NewReporter(t.TempDir(), "", true, false, false, nil, nil) // dry run

	filesChan := make(chan pipeline.ClassifiedFile, 1)
	errorsChan := make(chan pipeline.ComplianceError, 1)

	filesChan <- pipeline.ClassifiedFile{
		Path:          "third_party/foo/main.cc",
		ProjectRoot:   "third_party/foo",
		IsLicenseFile: false,
		Matches:       []pipeline.LicenseMatch{},
	}
	close(filesChan)
	close(errorsChan)

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	err := reporter.Run(ctx, filesChan, errorsChan)
	if err == nil {
		t.Fatal("Expected error due to missing license file, got nil")
	}

	if !strings.Contains(err.Error(), "1 compliance error") {
		t.Errorf("Expected error to contain error count, got: %v", err)
	}
	if !strings.Contains(err.Error(), "Project has no recognized license files") {
		t.Errorf("Expected error to contain missing license issue description, got: %v", err)
	}
}
