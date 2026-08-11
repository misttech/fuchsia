// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package report

import (
	"context"
	"os"
	"path/filepath"
	"testing"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
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

	v := NewTargetComplianceVerifier(tempDir, "foo.cc", nil)
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

	v := NewTargetComplianceVerifier(tempDir, "bar.cc", nil)
	if err := v.Run(context.Background(), []*pipeline.Project{proj}, nil); err == nil {
		t.Errorf("Expected error for undeclared license, got nil")
	}
}
