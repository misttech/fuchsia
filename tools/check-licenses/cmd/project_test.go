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

const mockMITLicenseText = `Permission is hereby granted, free of charge, to any person obtaining a copy`

const mockBSDLicenseText = `Redistribution and use in source and binary forms, with or without modification, are permitted provided that the following conditions are met:
	1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following disclaimer.
	2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the following disclaimer in the documentation and/or other materials provided with the distribution.
	3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote products derived from this software without specific prior written permission.
	THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.`

func TestProjectCommand_Check(t *testing.T) {
	tempDir := t.TempDir()

	// 1. Scaffold the fake assets/patterns directory so the v2 Classifier can load it
	mitPatternDir := filepath.Join(tempDir, "tools", "check-licenses", "assets", "patterns", "Permissive", "MIT")
	if err := os.MkdirAll(mitPatternDir, 0755); err != nil {
		t.Fatal(err)
	}
	// A simple recognizable MIT text snippet for the classifier
	if err := os.WriteFile(filepath.Join(mitPatternDir, "mit.txt"), []byte(mockMITLicenseText), 0644); err != nil {
		t.Fatal(err)
	}

	bsdPatternDir := filepath.Join(tempDir, "tools", "check-licenses", "assets", "patterns", "Permissive", "BSD")
	if err := os.MkdirAll(bsdPatternDir, 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(bsdPatternDir, "bsd.txt"), []byte("Redistribution and use in source and binary forms"), 0644); err != nil {
		t.Fatal(err)
	}

	// Scaffold configs dir to avoid Assemble errors
	os.MkdirAll(filepath.Join(tempDir, "tools", "check-licenses", "assets", "configs"), 0755)

	// 2. Create a third_party project with a README
	projectDir := filepath.Join(tempDir, "third_party", "foo")
	if err := os.MkdirAll(projectDir, 0755); err != nil {
		t.Fatal(err)
	}
	readmeContent := []byte("Name: foo\nURL: http://foo\nVersion: 1.0\nRevision: abc\nSecurity Critical: no\nLicense: MIT\nLicense File: declared.cc\n")
	if err := os.WriteFile(filepath.Join(projectDir, "README.fuchsia"), readmeContent, 0644); err != nil {
		t.Fatal(err)
	}

	// 3. Create a declared file with a license
	declaredFile := filepath.Join(projectDir, "declared.cc")
	if err := os.WriteFile(declaredFile, []byte("/* Permission is hereby granted, free of charge, to any person obtaining a copy */\nint main() {}"), 0644); err != nil {
		t.Fatal(err)
	}

	// 4. Create an undeclared file with a license
	undeclaredFile := filepath.Join(projectDir, "undeclared.cc")
	if err := os.WriteFile(undeclaredFile, []byte("/* Redistribution and use in source and binary forms */\nint helper() {}"), 0644); err != nil {
		t.Fatal(err)
	}

	// Initialize the command
	cmd := &ProjectCommand{
		fuchsiaDir: tempDir,
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	// Test 1: Declared file should pass
	fs1 := flag.NewFlagSet("test", flag.ContinueOnError)
	cmd.SetFlags(fs1)
	fs1.Parse([]string{"--fuchsia_dir", tempDir, "check", declaredFile})
	status := cmd.Execute(ctx, fs1)
	if status != subcommands.ExitSuccess {
		t.Errorf("Expected ExitSuccess for declared file, got %v", status)
	}

	// Test 2: Undeclared file should fail
	fs2 := flag.NewFlagSet("test", flag.ContinueOnError)
	cmd.SetFlags(fs2)
	fs2.Parse([]string{"--fuchsia_dir", tempDir, "check", undeclaredFile})
	status = cmd.Execute(ctx, fs2)
	if status != subcommands.ExitFailure {
		t.Errorf("Expected ExitFailure for undeclared file, got %v", status)
	}
}

func TestProjectCommand_Update(t *testing.T) {
	tempDir := t.TempDir()

	mitPatternDir := filepath.Join(tempDir, "tools", "check-licenses", "assets", "patterns", "Permissive", "MIT")
	if err := os.MkdirAll(mitPatternDir, 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(mitPatternDir, "mit.txt"), []byte(mockMITLicenseText), 0644); err != nil {
		t.Fatal(err)
	}

	bsdPatternDir := filepath.Join(tempDir, "tools", "check-licenses", "assets", "patterns", "Permissive", "BSD")
	if err := os.MkdirAll(bsdPatternDir, 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(bsdPatternDir, "bsd.txt"), []byte(mockBSDLicenseText), 0644); err != nil {
		t.Fatal(err)
	}

	os.MkdirAll(filepath.Join(tempDir, "tools", "check-licenses", "assets", "configs"), 0755)

	projectDir := filepath.Join(tempDir, "third_party", "foo")
	if err := os.MkdirAll(projectDir, 0755); err != nil {
		t.Fatal(err)
	}

	readmeContent := []byte("Name: foo\nURL: http://foo\nVersion: 1.0\nRevision: abc\nSecurity Critical: no\n\nNon-License File: ignored.cc\n")
	if err := os.WriteFile(filepath.Join(projectDir, "README.fuchsia"), readmeContent, 0644); err != nil {
		t.Fatal(err)
	}

	os.WriteFile(filepath.Join(projectDir, "ignored.cc"), []byte("/* Permission is hereby granted, free of charge, to any person obtaining a copy */\n"), 0644)
	os.WriteFile(filepath.Join(projectDir, "found.cc"), append([]byte("/*\n"), append([]byte(mockBSDLicenseText), []byte("\n*/\n")...)...), 0644)
	os.WriteFile(filepath.Join(projectDir, "LICENSE"), []byte("Permission is hereby granted, free of charge, to any person obtaining a copy\n"), 0644)

	cmd := &ProjectCommand{
		fuchsiaDir: tempDir,
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	fs := flag.NewFlagSet("test", flag.ContinueOnError)
	cmd.SetFlags(fs)
	fs.Parse([]string{"--fuchsia_dir", tempDir, "update", projectDir})
	status := cmd.Execute(ctx, fs)

	if status != subcommands.ExitSuccess {
		t.Errorf("Expected ExitSuccess for update, got %v", status)
	}

	updatedReadme, err := os.ReadFile(filepath.Join(projectDir, "README.fuchsia"))
	if err != nil {
		t.Fatal(err)
	}
	content := string(updatedReadme)

	if !strings.Contains(content, "License File: LICENSE") {
		t.Errorf("Expected LICENSE to be added to License File list, got:\n%s", content)
	}
	if !strings.Contains(content, "Non-License File: ignored.cc") {
		t.Errorf("Expected ignored.cc to be preserved in Non-License File list, got:\n%s", content)
	}
}

func TestProjectCommand_ListAndInfo(t *testing.T) {
	tempDir := t.TempDir()

	os.MkdirAll(filepath.Join(tempDir, "tools", "check-licenses", "assets", "configs"), 0755)

	mitPatternDir := filepath.Join(tempDir, "tools", "check-licenses", "assets", "patterns", "Permissive", "MIT")
	os.MkdirAll(mitPatternDir, 0755)
	os.WriteFile(filepath.Join(mitPatternDir, "mit.txt"), []byte(mockMITLicenseText), 0644)

	projectDir := filepath.Join(tempDir, "third_party", "foo")
	if err := os.MkdirAll(projectDir, 0755); err != nil {
		t.Fatal(err)
	}
	readmeContent := []byte("Name: foo\nURL: http://foo\nVersion: 1.0\nRevision: abc\nSecurity Critical: no\nLicense: MIT\nLicense File: declared.cc\n")
	if err := os.WriteFile(filepath.Join(projectDir, "README.fuchsia"), readmeContent, 0644); err != nil {
		t.Fatal(err)
	}

	cmd := &ProjectCommand{
		fuchsiaDir: tempDir,
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	// Test list
	fsList := flag.NewFlagSet("test", flag.ContinueOnError)
	cmd.SetFlags(fsList)
	fsList.Parse([]string{"--fuchsia_dir", tempDir, "list", tempDir})
	if status := cmd.Execute(ctx, fsList); status != subcommands.ExitSuccess {
		t.Errorf("Expected ExitSuccess for list, got %v", status)
	}

	// Test info
	fsInfo := flag.NewFlagSet("test", flag.ContinueOnError)
	cmd.SetFlags(fsInfo)
	fsInfo.Parse([]string{"--fuchsia_dir", tempDir, "info", filepath.Join(projectDir, "declared.cc")})
	if status := cmd.Execute(ctx, fsInfo); status != subcommands.ExitSuccess {
		t.Errorf("Expected ExitSuccess for info, got %v", status)
	}
}

func TestBelongsToProject(t *testing.T) {
	tempDir := t.TempDir()
	fuchsiaDir := tempDir

	// Setup a fake project structure
	// third_party/foo
	// third_party/foo/vendor/bar

	fooDir := filepath.Join(tempDir, "third_party", "foo")
	if err := os.MkdirAll(fooDir, 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(fooDir, "README.fuchsia"), []byte("Name: foo\n"), 0644); err != nil {
		t.Fatal(err)
	}

	barDir := filepath.Join(fooDir, "vendor", "bar")
	if err := os.MkdirAll(barDir, 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(barDir, "README.fuchsia"), []byte("Name: bar\n"), 0644); err != nil {
		t.Fatal(err)
	}

	// This is equivalent to outOfTreeReadmes
	outOfTree := map[string]string{}

	tests := []struct {
		name        string
		policyPath  string
		projectRoot string
		expected    bool
	}{
		{
			name:        "Exact match",
			policyPath:  "third_party/foo",
			projectRoot: "third_party/foo",
			expected:    true,
		},
		{
			name:        "Policy on parent directory is inherited",
			policyPath:  "third_party",
			projectRoot: "third_party/foo",
			expected:    true,
		},
		{
			name:        "Policy on unrelated directory is false",
			policyPath:  "src/other",
			projectRoot: "third_party/foo",
			expected:    false,
		},
		{
			name:        "Policy on sub-project should NOT apply to parent project",
			policyPath:  "third_party/foo/vendor/bar",
			projectRoot: "third_party/foo",
			expected:    false,
		},
		{
			name:        "Policy on sub-project should apply to sub-project itself",
			policyPath:  "third_party/foo/vendor/bar",
			projectRoot: "third_party/foo/vendor/bar",
			expected:    true,
		},
		{
			name:        "Policy on regular sub-directory without boundary SHOULD apply to parent",
			policyPath:  "third_party/foo/src",
			projectRoot: "third_party/foo",
			expected:    true,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			result := belongsToProject(tt.policyPath, tt.projectRoot, fuchsiaDir, outOfTree, make(map[string]*readmeResult))
			if result != tt.expected {
				t.Errorf("belongsToProject(%q, %q) = %v, want %v", tt.policyPath, tt.projectRoot, result, tt.expected)
			}
		})
	}
}

func TestProjectCommand_Update_UnclassifiedLicense(t *testing.T) {
	tempDir := t.TempDir()

	os.MkdirAll(filepath.Join(tempDir, "tools", "check-licenses", "assets", "configs"), 0755)

	projectDir := filepath.Join(tempDir, "third_party", "bar")
	if err := os.MkdirAll(projectDir, 0755); err != nil {
		t.Fatal(err)
	}

	os.WriteFile(filepath.Join(projectDir, "LICENSE"), []byte("Some completely unclassified proprietary text\n"), 0644)

	readmeContent := []byte("Name: bar\nURL: http://bar\nVersion: 1.0\nRevision: abc\nSecurity Critical: no\n\nLicense File: LICENSE\n")
	if err := os.WriteFile(filepath.Join(projectDir, "README.fuchsia"), readmeContent, 0644); err != nil {
		t.Fatal(err)
	}

	cmd := &ProjectCommand{
		fuchsiaDir: tempDir,
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	fs := flag.NewFlagSet("test", flag.ContinueOnError)
	cmd.SetFlags(fs)
	fs.Parse([]string{"--fuchsia_dir", tempDir, "update", projectDir})
	if status := cmd.Execute(ctx, fs); status != subcommands.ExitSuccess {
		t.Errorf("Expected ExitSuccess for update, got %v", status)
	}

	updatedReadme, err := os.ReadFile(filepath.Join(projectDir, "README.fuchsia"))
	if err != nil {
		t.Fatal(err)
	}
	content := string(updatedReadme)

	if !strings.Contains(content, "License File: LICENSE") || !strings.Contains(content, "License: Unclassified") {
		t.Errorf("Expected LICENSE to be retained with Unclassified license, got:\n%s", content)
	}
}
