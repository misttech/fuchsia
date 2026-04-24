// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package validate

import (
	"context"
	"path/filepath"
	"strings"
	"testing"
	"time"

	v2config "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/config"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
)

func TestValidator_Run(t *testing.T) {
	fuchsiaDir := t.TempDir()

	policyExceptions := map[string]map[string]v2config.RuleMetadata{
		"AllLicenseTextsMustBeRecognized": {
			"third_party/foo/LICENSE": v2config.RuleMetadata{Bug: "test", Description: "test"},
		},
		"AllFuchsiaAuthorSourceFilesMustHaveCopyrightHeaders": {
			"src/legacy/old.cc": v2config.RuleMetadata{Bug: "test", Description: "test"},
		},
	}

	allowedLicenses := map[string]map[string]v2config.RuleMetadata{
		"GPL-2.0": {
			"third_party/legacy_gpl/LICENSE": v2config.RuleMetadata{Bug: "test", Description: "test"},
		},
	}

	validator := NewValidator(fuchsiaDir, policyExceptions, allowedLicenses)

	inChan := make(chan pipeline.ClassifiedFile, 15)

	// 1. Valid License File
	inChan <- pipeline.ClassifiedFile{
		Path:          filepath.Join(fuchsiaDir, "LICENSE"),
		IsLicenseFile: true,
		Matches:       []pipeline.LicenseMatch{{SPDXID: "MIT", MatchType: "Permissive"}},
	}

	// 2. Invalid License File (No matches, not allowlisted)
	inChan <- pipeline.ClassifiedFile{
		Path:          filepath.Join(fuchsiaDir, "third_party", "bar", "LICENSE"),
		ProjectRoot:   filepath.Join(fuchsiaDir, "third_party", "bar"),
		IsLicenseFile: true,
		Matches:       []pipeline.LicenseMatch{},
	}

	// 3. Invalid License File (No matches, BUT allowlisted)
	inChan <- pipeline.ClassifiedFile{
		Path:          filepath.Join(fuchsiaDir, "third_party", "foo", "LICENSE"),
		ProjectRoot:   filepath.Join(fuchsiaDir, "third_party", "foo"),
		IsLicenseFile: true,
		Matches:       []pipeline.LicenseMatch{},
	}

	// 4. Valid Source File
	inChan <- pipeline.ClassifiedFile{
		Path:          filepath.Join(fuchsiaDir, "src", "main.cc"),
		ProjectRoot:   fuchsiaDir,
		IsLicenseFile: false,
		Matches:       []pipeline.LicenseMatch{{SPDXID: "FuchsiaCopyright", MatchType: "Copyright"}},
	}

	// 5. Invalid Source File (No copyright, not allowlisted)
	inChan <- pipeline.ClassifiedFile{
		Path:          filepath.Join(fuchsiaDir, "src", "bad.cc"),
		ProjectRoot:   fuchsiaDir,
		IsLicenseFile: false,
		Matches:       []pipeline.LicenseMatch{{SPDXID: "MIT", MatchType: "Permissive"}}, // MIT is not FuchsiaCopyright
	}

	// 6. Invalid Source File (No copyright, BUT allowlisted)
	inChan <- pipeline.ClassifiedFile{
		Path:          filepath.Join(fuchsiaDir, "src", "legacy", "old.cc"),
		ProjectRoot:   fuchsiaDir,
		IsLicenseFile: false,
		Matches:       []pipeline.LicenseMatch{},
	}

	// 7. Non-Fuchsia Source File (No copyright, but it's third-party)
	inChan <- pipeline.ClassifiedFile{
		Path:          filepath.Join(fuchsiaDir, "third_party", "foo", "main.cc"),
		ProjectRoot:   filepath.Join(fuchsiaDir, "third_party", "foo"),
		IsLicenseFile: false,
		Matches:       []pipeline.LicenseMatch{},
	}

	// 8. Non-source extension (No copyright, not allowlisted, but extension skips check)
	inChan <- pipeline.ClassifiedFile{
		Path:          filepath.Join(fuchsiaDir, "src", "image.jpg"),
		ProjectRoot:   fuchsiaDir,
		IsLicenseFile: false,
		Matches:       []pipeline.LicenseMatch{},
	}

	// 9. Restricted License File (Not allowlisted)
	inChan <- pipeline.ClassifiedFile{
		Path:          filepath.Join(fuchsiaDir, "third_party", "bad_gpl", "LICENSE"),
		ProjectRoot:   filepath.Join(fuchsiaDir, "third_party", "bad_gpl"),
		IsLicenseFile: true,
		Matches:       []pipeline.LicenseMatch{{SPDXID: "GPL-2.0", MatchType: "Restricted"}},
	}

	// 10. Restricted License File (Allowlisted)
	inChan <- pipeline.ClassifiedFile{
		Path:          filepath.Join(fuchsiaDir, "third_party", "legacy_gpl", "LICENSE"),
		ProjectRoot:   filepath.Join(fuchsiaDir, "third_party", "legacy_gpl"),
		IsLicenseFile: true,
		Matches:       []pipeline.LicenseMatch{{SPDXID: "GPL-2.0", MatchType: "Restricted"}},
	}

	// 11. Empty __init__.py (Exempted)
	inChan <- pipeline.ClassifiedFile{
		Path:          filepath.Join(fuchsiaDir, "src", "__init__.py"),
		ProjectRoot:   fuchsiaDir,
		IsLicenseFile: false,
		Matches:       []pipeline.LicenseMatch{},
		AnalyzedText:  []byte{},
	}

	// 12. Non-empty __init__.py (Not exempted)
	inChan <- pipeline.ClassifiedFile{
		Path:          filepath.Join(fuchsiaDir, "src", "sub", "__init__.py"),
		ProjectRoot:   fuchsiaDir,
		IsLicenseFile: false,
		Matches:       []pipeline.LicenseMatch{},
		AnalyzedText:  []byte("print('hello')"),
	}

	close(inChan)

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	outChan, err := validator.Run(ctx, inChan)
	if err != nil {
		t.Fatalf("Failed to run validator: %v", err)
	}

	var errors []pipeline.ComplianceError
	for err := range outChan {
		errors = append(errors, err)
	}

	if len(errors) != 4 {
		t.Fatalf("Expected exactly 4 compliance errors, got %d: %v", len(errors), errors)
	}

	hasUnrecognizedLicenseErr := false
	hasMissingCopyrightErr := false
	hasMissingInitCopyrightErr := false
	hasUnapprovedPatternErr := false

	for _, e := range errors {
		if strings.Contains(e.Issue, "Unrecognized license text") {
			if e.FilePath != filepath.Join(fuchsiaDir, "third_party", "bar", "LICENSE") {
				t.Errorf("Unexpected unrecognized license error for file: %s", e.FilePath)
			}
			hasUnrecognizedLicenseErr = true
		}
		if strings.Contains(e.Issue, "Missing Fuchsia copyright header") {
			if e.FilePath == filepath.Join(fuchsiaDir, "src", "bad.cc") {
				hasMissingCopyrightErr = true
			} else if e.FilePath == filepath.Join(fuchsiaDir, "src", "sub", "__init__.py") {
				hasMissingInitCopyrightErr = true
			} else {
				t.Errorf("Unexpected missing copyright error for file: %s", e.FilePath)
			}
		}
		if strings.Contains(e.Issue, "was not approved to use license pattern") {
			if e.FilePath != filepath.Join(fuchsiaDir, "third_party", "bad_gpl", "LICENSE") {
				t.Errorf("Unexpected unapproved pattern error for file: %s", e.FilePath)
			}
			hasUnapprovedPatternErr = true
		}
	}

	if !hasUnrecognizedLicenseErr {
		t.Error("Expected unrecognized license error, but it was not emitted")
	}
	if !hasMissingCopyrightErr {
		t.Error("Expected missing copyright error for bad.cc, but it was not emitted")
	}
	if !hasMissingInitCopyrightErr {
		t.Error("Expected missing copyright error for non-empty __init__.py, but it was not emitted")
	}
	if !hasUnapprovedPatternErr {
		t.Error("Expected unapproved pattern error, but it was not emitted")
	}
}
