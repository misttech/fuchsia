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

	"go.fuchsia.dev/fuchsia/tools/check-licenses/pipeline"
	"go.fuchsia.dev/fuchsia/tools/readme_fuchsia"
)

func TestTargetComplianceVerifier_Declared(t *testing.T) {
	tempDir := t.TempDir()
	sourceFile := filepath.Join(tempDir, "foo.cc")
	os.WriteFile(sourceFile, []byte("int main() {}"), 0644)
	licenseFile := filepath.Join(tempDir, "LICENSE")
	os.WriteFile(licenseFile, []byte("LICENSE"), 0644)

	orig := &readme_fuchsia.Readme{
		Name:             "foo",
		URL:              "http://foo",
		Version:          "1.0",
		Revision:         "123",
		SecurityCritical: "no",
		Licenses:         []string{"MIT"},
		LicenseFiles:     []string{"LICENSE"},
	}
	clone := *orig

	proj := &pipeline.Project{
		RootPath: tempDir,
		Readme: &pipeline.ReadmeFile{
			Path: filepath.Join(tempDir, "README.fuchsia"),
			Segments: []*pipeline.ReadmeSegment{
				{Original: orig, Updated: &clone},
			},
		},
		ClassifiedFiles: []pipeline.ClassifiedFile{
			{
				Path:        sourceFile,
				ProjectRoot: tempDir,
				Matches: []pipeline.LicenseMatch{
					{SPDXID: "MIT", MatchType: "Notice"},
				},
			},
		},
	}

	v := NewTargetComplianceVerifier(tempDir, nil, "foo.cc")
	if err := v.Run(context.Background(), []*pipeline.Project{proj}, nil); err != nil {
		t.Errorf("Expected success for declared license, got: %v", err)
	}
}

func TestTargetComplianceVerifier_Undeclared(t *testing.T) {
	tempDir := t.TempDir()
	sourceFile := filepath.Join(tempDir, "bar.cc")
	os.WriteFile(sourceFile, []byte("int main() {}"), 0644)
	licenseFile := filepath.Join(tempDir, "LICENSE")
	os.WriteFile(licenseFile, []byte("LICENSE"), 0644)

	orig := &readme_fuchsia.Readme{
		Name:             "foo",
		URL:              "http://foo",
		Version:          "1.0",
		Revision:         "123",
		SecurityCritical: "no",
		Licenses:         []string{"MIT"},
		LicenseFiles:     []string{"LICENSE"},
	}
	clone := *orig

	proj := &pipeline.Project{
		RootPath: tempDir,
		Readme: &pipeline.ReadmeFile{
			Path: filepath.Join(tempDir, "README.fuchsia"),
			Segments: []*pipeline.ReadmeSegment{
				{Original: orig, Updated: &clone},
			},
		},
		ClassifiedFiles: []pipeline.ClassifiedFile{
			{
				Path:        sourceFile,
				ProjectRoot: tempDir,
				Matches: []pipeline.LicenseMatch{
					{SPDXID: "GPL-2.0", MatchType: "Restricted"},
				},
			},
		},
	}

	v := NewTargetComplianceVerifier(tempDir, nil, "bar.cc")
	if err := v.Run(context.Background(), []*pipeline.Project{proj}, nil); err == nil {
		t.Errorf("Expected error for undeclared license, got nil")
	}
}

func TestTargetComplianceVerifier_FirstParty(t *testing.T) {
	tempDir := t.TempDir()
	sourceFile := filepath.Join(tempDir, "src", "lib", "valid.cc")
	os.MkdirAll(filepath.Dir(sourceFile), 0755)
	os.WriteFile(sourceFile, []byte("int main() {}"), 0644)
	licenseFile := filepath.Join(tempDir, "LICENSE")
	os.WriteFile(licenseFile, []byte("LICENSE"), 0644)

	orig := &readme_fuchsia.Readme{
		Name:             "Fuchsia",
		SecurityCritical: "yes",
		FirstParty:       "yes",
		Licenses:         []string{"BSD-2-Clause"},
		LicenseFiles:     []string{"LICENSE"},
	}
	clone := *orig

	proj := &pipeline.Project{
		RootPath: tempDir,
		Readme: &pipeline.ReadmeFile{
			Path: filepath.Join(tempDir, "tools", "check-licenses", "assets", "readmes", "README.fuchsia"),
			Segments: []*pipeline.ReadmeSegment{
				{Original: orig, Updated: &clone},
			},
		},
		ClassifiedFiles: []pipeline.ClassifiedFile{
			{
				Path:         sourceFile,
				ProjectRoot:  tempDir,
				IsFirstParty: true,
			},
		},
	}

	v := NewTargetComplianceVerifier(tempDir, nil, "src/lib/valid.cc")
	if err := v.Run(context.Background(), []*pipeline.Project{proj}, nil); err != nil {
		t.Errorf("Expected success for 1st-party source file, got: %v", err)
	}
}

func TestTargetComplianceVerifier_ComplianceErrors(t *testing.T) {
	tempDir := t.TempDir()
	sourceFile := filepath.Join(tempDir, "bad.cc")
	os.WriteFile(sourceFile, []byte("int main() {}"), 0644)

	complianceErr := pipeline.ComplianceError{
		CheckName: "AllFuchsiaAuthorSourceFilesMustHaveCopyrightHeaders",
		Project:   tempDir,
		FilePath:  sourceFile,
		Issue:     "Missing Fuchsia copyright header in first-party source file.",
	}

	v := NewTargetComplianceVerifier(tempDir, nil, "bad.cc")
	err := v.Run(context.Background(), nil, []pipeline.ComplianceError{complianceErr})
	if err == nil {
		t.Fatal("Expected error when compliance error is present for target, got nil")
	}
	if !strings.Contains(err.Error(), "Missing Fuchsia copyright header") {
		t.Errorf("Expected error to contain copyright header message, got: %v", err)
	}
}
