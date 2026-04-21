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
)

func TestCopyrightCommand_Execute(t *testing.T) {
	// Create a temporary workspace to simulate a Fuchsia checkout
	tempDir := t.TempDir()

	// 1. Scaffold the fake assets/patterns directory so the v2 Classifier can load it
	patternDir := filepath.Join(tempDir, "tools", "check-licenses", "assets", "patterns", "_Header", "FuchsiaCopyright")
	if err := os.MkdirAll(patternDir, 0755); err != nil {
		t.Fatal(err)
	}
	patternContent := []byte(`Copyright 2026 The Fuchsia Authors. All rights reserved.
Use of this source code is governed by a BSD-style license that can be
found in the LICENSE file.`)
	if err := os.WriteFile(filepath.Join(patternDir, "fuchsia.txt"), patternContent, 0644); err != nil {
		t.Fatal(err)
	}

	// 2. Create a test file WITHOUT a copyright header
	testFilePath := filepath.Join(tempDir, "test.cc")
	if err := os.WriteFile(testFilePath, []byte("int main() { return 0; }\n"), 0644); err != nil {
		t.Fatal(err)
	}

	// Initialize the command
	cmd := &CopyrightCommand{
		fuchsiaDir:  tempDir,
		printStdout: false, // Initially just checking and overwriting
	}

	// Simulate parsing flags (the flag package handles creating the struct)
	fs := flag.NewFlagSet("test", flag.ContinueOnError)
	cmd.SetFlags(fs)
	fs.Parse([]string{testFilePath})

	// 3. Test Add (Remediation - Default behavior)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	status := cmd.Execute(ctx, fs)
	if status != 0 { // subcommands.ExitSuccess is 0
		t.Errorf("Expected ExitSuccess (0) after adding copyright, got %v", status)
	}

	// Verify the file was actually modified on disk
	contentBytes, err := os.ReadFile(testFilePath)
	if err != nil {
		t.Fatal(err)
	}
	content := string(contentBytes)
	if !strings.Contains(content, "Copyright") {
		t.Error("Expected file to be modified with 'Copyright', but it was not found")
	}
	if !strings.Contains(content, "//") {
		t.Errorf("Expected C++ file to use '//' comment syntax, got: %s", content)
	}

	// 4. Test Check (Existing - Default behavior)
	status = cmd.Execute(ctx, fs)
	if status != 0 {
		t.Errorf("Expected ExitSuccess (0) for existing copyright, got %v", status)
	}

	// 5. Test stdout (SHAC mode)
	cmd.printStdout = true
	status = cmd.Execute(ctx, fs)
	if status != 0 {
		t.Errorf("Expected ExitSuccess (0) for existing copyright, got %v", status)
	}
}

func TestCopyrightCommand_CommentPrefixes(t *testing.T) {
	tempDir := t.TempDir()

	tests := []struct {
		name        string
		ext         string
		expected    string
		expectError bool
	}{
		{"Python script", ".py", "#", false},
		{"Shell script", ".sh", "#", false},
		{"GN build", ".gn", "#", false},
		{"C++ source", ".cc", "//", false},
		{"Rust source", ".rs", "//", false},
		{"Assembly", ".asm", ";", false},
		{"Batch script", ".bat", "rem", false},
		{"Unknown extension", ".unknown", "", true},
		{"JSON file", ".json", "", true},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			filePath := filepath.Join(tempDir, "test_file"+tt.ext)
			if err := os.WriteFile(filePath, []byte("code()\n"), 0644); err != nil {
				t.Fatal(err)
			}

			newBytes, err := addCopyright(filePath)
			if tt.expectError {
				if err == nil {
					t.Errorf("Expected error for extension %s, got nil", tt.ext)
				}
				return
			}

			if err != nil {
				t.Fatalf("Unexpected error for extension %s: %v", tt.ext, err)
			}

			content := string(newBytes)
			if !strings.HasPrefix(content, tt.expected+" Copyright") {
				t.Errorf("Expected comment prefix '%s', got content:\n%s", tt.expected, content)
			}
		})
	}
}
