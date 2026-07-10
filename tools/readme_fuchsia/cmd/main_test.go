// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"bytes"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestRunValidate_FailureMessage(t *testing.T) {
	tmpDir := t.TempDir()
	readmePath := filepath.Join(tmpDir, "README.fuchsia")

	// Missing required field 'Security Critical'
	content := `Name: test_lib
URL: https://example.com
Revision: 12345
License: MIT
License File: LICENSE
`
	if err := os.WriteFile(readmePath, []byte(content), 0644); err != nil {
		t.Fatalf("failed to write test file: %v", err)
	}
	licensePath := filepath.Join(tmpDir, "LICENSE")
	if err := os.WriteFile(licensePath, []byte("MIT License"), 0644); err != nil {
		t.Fatalf("failed to write license file: %v", err)
	}

	oldStderr := os.Stderr
	r, w, _ := os.Pipe()
	os.Stderr = w

	err := runValidate([]string{readmePath})
	w.Close()
	os.Stderr = oldStderr

	var buf bytes.Buffer
	buf.ReadFrom(r)
	output := buf.String()

	if err == nil {
		t.Fatal("expected validation error, got nil")
	}
	if err.Error() != "" {
		t.Errorf("expected empty error string on validation failure, got: %q", err.Error())
	}

	lines := strings.Split(strings.TrimSpace(output), "\n")
	if len(lines) < 2 {
		t.Fatalf("expected at least 2 lines of stderr output, got %d: %q", len(lines), output)
	}
	if !strings.HasPrefix(lines[0], "validation failed for") {
		t.Errorf("expected first line to start with 'validation failed for', got: %q", lines[0])
	}
	if !strings.Contains(lines[1], "Missing required field 'Security Critical'") {
		t.Errorf("expected second line to contain \"Missing required field 'Security Critical'\", got: %q", lines[1])
	}
	if !strings.Contains(lines[1], "http://go/readme_fuchsia#security-critical") {
		t.Errorf("expected second line to contain short link 'http://go/readme_fuchsia#security-critical', got: %q", lines[1])
	}
}

func TestRunValidate_Success(t *testing.T) {
	tmpDir := t.TempDir()
	readmePath := filepath.Join(tmpDir, "README.fuchsia")

	content := `Name: test_lib
URL: https://example.com
Revision: 12345
Security Critical: no
License: MIT
License File: LICENSE
`
	if err := os.WriteFile(readmePath, []byte(content), 0644); err != nil {
		t.Fatalf("failed to write test file: %v", err)
	}
	licensePath := filepath.Join(tmpDir, "LICENSE")
	if err := os.WriteFile(licensePath, []byte("MIT License"), 0644); err != nil {
		t.Fatalf("failed to write license file: %v", err)
	}

	if err := runValidate([]string{readmePath}); err != nil {
		t.Fatalf("expected validation success, got: %v", err)
	}
}
