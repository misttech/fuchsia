// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package result

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// =========================================================================
// Template Generation Tests (template.go)
// =========================================================================

func TestInitializeTemplates(t *testing.T) {
	tempDir := t.TempDir()
	Config = NewConfig()
	Config.FuchsiaDir = tempDir

	tmplDir := filepath.Join(tempDir, "templates")
	os.MkdirAll(tmplDir, 0755)

	// Create a dummy template file
	tmplPath := filepath.Join(tmplDir, "NOTICE.txt")
	os.WriteFile(tmplPath, []byte("Notice Template"), 0644)

	Config.Templates = []*Template{
		{Paths: []string{"templates"}},
	}

	err := initializeTemplates()
	if err != nil {
		t.Fatalf("Failed to initialize templates: %v", err)
	}

	if _, ok := AllTemplates["NOTICE.txt"]; !ok {
		t.Error("Expected NOTICE.txt template to be loaded into global map")
	}
}

func TestExpandTemplates(t *testing.T) {
	tempDir := t.TempDir()
	Config = NewConfig()
	Config.FuchsiaDir = tempDir
	Config.OutDir = filepath.Join(tempDir, "out_dir")
	os.MkdirAll(Config.OutDir, 0755)

	// Re-initialize a template directly for the test
	initializeTemplatesForTest(tempDir)

	Config.Outputs = []string{"NOTICE.txt"}

	// Expand without zip
	out, err := expandTemplates()
	if err != nil {
		t.Fatalf("Failed to expand templates: %v", err)
	}

	if !strings.Contains(out, "Executed template ->") {
		t.Errorf("Expected output log to contain execution confirmation, got %q", out)
	}

	// Verify file was written
	writtenPath := filepath.Join(Config.OutDir, "out", "NOTICE.txt")
	content, err := os.ReadFile(writtenPath)
	if err != nil {
		t.Fatalf("Expected file to be written to disk: %v", err)
	}

	if string(content) != "Output: " { // world struct empty, so just Output:
		t.Errorf("Expected template content 'Output: ', got %q", string(content))
	}
}

func TestExpandTemplates_Zip(t *testing.T) {
	tempDir := t.TempDir()
	Config = NewConfig()
	Config.FuchsiaDir = tempDir
	Config.OutDir = filepath.Join(tempDir, "out_dir")
	os.MkdirAll(Config.OutDir, 0755)
	Config.Zip = true

	initializeTemplatesForTest(tempDir)
	Config.Outputs = []string{"NOTICE.txt"}

	_, err := expandTemplates()
	if err != nil {
		t.Fatalf("Failed to expand and zip templates: %v", err)
	}

	// Verify zip file was written
	zipPath := filepath.Join(Config.OutDir, "out", "NOTICE.txt.gz")
	if _, err := os.Stat(zipPath); os.IsNotExist(err) {
		t.Errorf("Expected gzipped template file to be generated at %s", zipPath)
	}
}

func initializeTemplatesForTest(tempDir string) {
	tmplDir := filepath.Join(tempDir, "templates")
	os.MkdirAll(tmplDir, 0755)

	// A simple template that just prints a hardcoded string
	tmplPath := filepath.Join(tmplDir, "NOTICE.txt")
	os.WriteFile(tmplPath, []byte("Output: "), 0644)

	Config.Templates = []*Template{
		{Paths: []string{"templates"}},
	}
	_ = initializeTemplates()
}
