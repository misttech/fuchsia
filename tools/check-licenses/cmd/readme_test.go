// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"context"
	"flag"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/google/subcommands"
)

func TestReadmeCommand_Format(t *testing.T) {
	tempDir := t.TempDir()
	testFilePath := filepath.Join(tempDir, "README.fuchsia")

	// Enforce test hermeticity by setting FUCHSIA_DIR to tempDir
	os.Setenv("FUCHSIA_DIR", tempDir)
	defer os.Unsetenv("FUCHSIA_DIR")

	// Messy unformatted content
	content := []byte(`
Name: test_project
URL: https://test
Version: 1.0
Security Critical: no
License File: LICENSE
    License: MIT
Description:
  A test project
`)
	if err := os.WriteFile(testFilePath, content, 0644); err != nil {
		t.Fatal(err)
	}

	cmd := &ReadmeCommand{}
	fs := flag.NewFlagSet("test", flag.ContinueOnError)
	cmd.SetFlags(fs)
	fs.Parse([]string{"format", testFilePath})

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	status := cmd.Execute(ctx, fs)
	if status != subcommands.ExitSuccess {
		t.Errorf("Expected ExitSuccess (0) for format action, got %v", status)
	}

	formattedBytes, err := os.ReadFile(testFilePath)
	if err != nil {
		t.Fatal(err)
	}
	formatted := string(formattedBytes)

	// It should now be canonically formatted (e.g. no weird leading newlines, fixed indents)
	if !strings.Contains(formatted, "Name: test_project") || !strings.Contains(formatted, "  License: MIT") {
		t.Errorf("File was not correctly formatted: %s", formatted)
	}
}

func TestReadmeCommand_Check(t *testing.T) {
	tempDir := t.TempDir()
	testFilePath := filepath.Join(tempDir, "README.fuchsia")

	// Enforce test hermeticity by setting FUCHSIA_DIR to tempDir
	os.Setenv("FUCHSIA_DIR", tempDir)
	defer os.Unsetenv("FUCHSIA_DIR")

	// Clean, canonical content
	content := []byte("Name: test_project\nURL: https://test\nVersion: 1.0\nSecurity Critical: no\n\nLicense File: LICENSE\n  License: MIT\n\nDescription:\n  A test project\n")
	if err := os.WriteFile(testFilePath, content, 0644); err != nil {
		t.Fatal(err)
	}

	cmd := &ReadmeCommand{}
	fs := flag.NewFlagSet("test", flag.ContinueOnError)
	cmd.SetFlags(fs)
	fs.Parse([]string{"check", testFilePath})

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	status := cmd.Execute(ctx, fs)
	if status != subcommands.ExitSuccess {
		t.Errorf("Expected ExitSuccess (0) for checking perfectly formatted file, got %v", status)
	}

	// Now introduce an unknown field to make it fail the check
	badContent := []byte("Name: test_project\nURL: https://test\nVersion: 1.0\nSecurity Critical: no\nLicense File: LICENSE\n  License: MIT\nUnknown Field: foo\nDescription:\n  A test project\n")
	if err := os.WriteFile(testFilePath, badContent, 0644); err != nil {
		t.Fatal(err)
	}

	fs.Parse([]string{"check", testFilePath})
	status = cmd.Execute(ctx, fs)
	if status != subcommands.ExitFailure {
		t.Errorf("Expected ExitFailure (1) for checking file with unknown field, got %v", status)
	}

	// Test missing Location for sub-project
	badSubProjectContent := []byte("Name: test_project\nURL: https://test\nVersion: 1.0\nSecurity Critical: no\nLicense File: LICENSE\n  License: MIT\n\n-------------------- DEPENDENCY DIVIDER --------------------\n\nName: sub\nURL: https://sub\nVersion: 1.0\nSecurity Critical: no\nLicense File: sub/LICENSE\n  License: MIT\n")
	if err := os.WriteFile(testFilePath, badSubProjectContent, 0644); err != nil {
		t.Fatal(err)
	}
	fs.Parse([]string{"check", testFilePath})
	status = cmd.Execute(ctx, fs)
	if status != subcommands.ExitFailure {
		t.Errorf("Expected ExitFailure (1) for checking file with missing Location on sub-project, got %v", status)
	}
}
