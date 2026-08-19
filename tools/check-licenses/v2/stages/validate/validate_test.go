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

	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
)

func TestValidator_Run(t *testing.T) {
	fuchsiaDir := t.TempDir()

	policyExceptions := map[string]map[string]RuleMetadata{
		"AllLicenseTextsMustBeRecognized": {
			"third_party/foo/LICENSE": RuleMetadata{Bug: "test", Description: "test"},
		},
		"AllFuchsiaAuthorSourceFilesMustHaveCopyrightHeaders": {
			"src/legacy/old.cc": RuleMetadata{Bug: "test", Description: "test"},
		},
		"AllProjectsMustHaveALicense": {
			"third_party/foo": RuleMetadata{Bug: "test", Description: "test"},
			"third_party/bar": RuleMetadata{Bug: "test", Description: "test"},
		},
	}

	allowedLicenses := map[string]map[string]RuleMetadata{
		"GPL-2.0": {
			"third_party/legacy_gpl/LICENSE": RuleMetadata{Bug: "test", Description: "test"},
		},
	}

	copyrightExtensions := map[string]bool{
		".cc": true,
		".py": true,
	}

	validator := NewValidator(fuchsiaDir, Config{
		PolicyExceptions:    policyExceptions,
		AllowedLicenses:     allowedLicenses,
		CopyrightExtensions: copyrightExtensions,
	})

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
		HasReadme:     true,
		Matches:       []pipeline.LicenseMatch{},
	}

	// 3. Invalid License File (No matches, BUT allowlisted)
	inChan <- pipeline.ClassifiedFile{
		Path:          filepath.Join(fuchsiaDir, "third_party", "foo", "LICENSE"),
		ProjectRoot:   filepath.Join(fuchsiaDir, "third_party", "foo"),
		IsLicenseFile: true,
		HasReadme:     true,
		Matches:       []pipeline.LicenseMatch{},
	}

	// 4. Valid Source File
	inChan <- pipeline.ClassifiedFile{
		Path:          filepath.Join(fuchsiaDir, "src", "main.cc"),
		ProjectRoot:   fuchsiaDir,
		IsLicenseFile: false,
		Matches:       []pipeline.LicenseMatch{{SPDXID: "FuchsiaCopyright", MatchType: "Copyright"}},
		AnalyzedText:  []byte("// Copyright 2026 The Fuchsia Authors. All rights reserved.\n// Use of this source code is governed by a BSD-style license that can be\n// found in the LICENSE file.\n"),
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
		HasReadme:     true,
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
		HasReadme:     true,
		Matches:       []pipeline.LicenseMatch{{SPDXID: "GPL-2.0", MatchType: "Restricted"}},
	}

	// 10. Restricted License File (Allowlisted)
	inChan <- pipeline.ClassifiedFile{
		Path:          filepath.Join(fuchsiaDir, "third_party", "legacy_gpl", "LICENSE"),
		ProjectRoot:   filepath.Join(fuchsiaDir, "third_party", "legacy_gpl"),
		IsLicenseFile: true,
		HasReadme:     true,
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

func TestValidator_RunFailure_MissingLicense(t *testing.T) {
	fuchsiaDir := t.TempDir()
	validator := NewValidator(fuchsiaDir, Config{})

	inChan := make(chan pipeline.ClassifiedFile, 1)

	inChan <- pipeline.ClassifiedFile{
		Path:          filepath.Join(fuchsiaDir, "third_party/foo/main.cc"),
		ProjectRoot:   filepath.Join(fuchsiaDir, "third_party/foo"),
		IsLicenseFile: false,
		HasReadme:     true,
		Matches:       []pipeline.LicenseMatch{},
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

	if len(errors) != 1 {
		t.Fatalf("Expected 1 error due to missing license file, got %d: %v", len(errors), errors)
	}

	if errors[0].CheckName != PolicyNoLicense {
		t.Errorf("Expected check name %s, got: %s", PolicyNoLicense, errors[0].CheckName)
	}
	if !strings.Contains(errors[0].Issue, "Project has no recognized license files") {
		t.Errorf("Expected error to contain missing license issue description, got: %v", errors[0].Issue)
	}
}

func TestValidator_RunFailure_MissingReadme(t *testing.T) {
	fuchsiaDir := t.TempDir()
	validator := NewValidator(fuchsiaDir, Config{})

	inChan := make(chan pipeline.ClassifiedFile, 1)

	inChan <- pipeline.ClassifiedFile{
		Path:          filepath.Join(fuchsiaDir, "third_party/foo/LICENSE"),
		ProjectRoot:   filepath.Join(fuchsiaDir, "third_party/foo"),
		IsLicenseFile: true,
		HasReadme:     false,
		Matches:       []pipeline.LicenseMatch{{SPDXID: "Apache-2.0", MatchType: "Approved"}},
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

	if len(errors) != 1 {
		t.Fatalf("Expected 1 error due to missing readme, got %d: %v", len(errors), errors)
	}

	if errors[0].CheckName != PolicyNoReadme {
		t.Errorf("Expected check name %s, got: %s", PolicyNoReadme, errors[0].CheckName)
	}
	if !strings.Contains(errors[0].Issue, "Third-party project is missing a README.fuchsia file") {
		t.Errorf("Expected error to contain missing readme issue description, got: %v", errors[0].Issue)
	}
}
