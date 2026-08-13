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

	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/config"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/v2/pipeline"
)

func TestPolicyRenderer_Run(t *testing.T) {
	tempDir := t.TempDir()
	cfg := config.NewMasterConfig(tempDir)
	renderer := NewPolicyRenderer(cfg)

	errors := []pipeline.ComplianceError{
		{
			CheckName: config.PolicyCheckAllProjectsMustHaveALicense,
			Project:   "src/foo/bar",
		},
		{
			CheckName: config.PolicyCheckAllLicenseTextsMustBeRecognized,
			FilePath:  "third_party/some_lib/LICENSE",
		},
	}

	if err := renderer.Run(context.Background(), nil, errors); err != nil {
		t.Fatalf("Expected Run to succeed, got: %v", err)
	}

	// 1. Verify project exception
	baseName1 := cfg.FindProjectBasename("src/foo/bar")
	expectedConfig1 := filepath.Join(tempDir, "tools", "check-licenses", "assets", "configs", "policy_exceptions", config.PolicyCheckAllProjectsMustHaveALicense, baseName1+".json")
	content1, err := os.ReadFile(expectedConfig1)
	if err != nil {
		t.Fatalf("Failed to read generated policy exception config at %s: %v", expectedConfig1, err)
	}
	if !strings.Contains(string(content1), "src/foo/bar") {
		t.Errorf("Expected config to contain 'src/foo/bar', got:\n%s", string(content1))
	}

	// 2. Verify unrecognized license exception
	baseName2 := cfg.FindProjectBasename("third_party/some_lib/LICENSE")
	expectedConfig2 := filepath.Join(tempDir, "tools", "check-licenses", "assets", "configs", "policy_exceptions", config.PolicyCheckAllLicenseTextsMustBeRecognized, baseName2+".json")
	content2, err := os.ReadFile(expectedConfig2)
	if err != nil {
		t.Fatalf("Failed to read generated policy exception config at %s: %v", expectedConfig2, err)
	}
	if !strings.Contains(string(content2), "third_party/some_lib/LICENSE") {
		t.Errorf("Expected config to contain 'third_party/some_lib/LICENSE', got:\n%s", string(content2))
	}
}
