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

func TestProjectCommand_Check(t *testing.T) {
	tempDir := t.TempDir()

	// 1. Scaffold the fake assets/patterns directory so the v2 Classifier can load it
	patternDir := filepath.Join(tempDir, "tools", "check-licenses", "assets", "patterns", "Permissive", "MIT")
	if err := os.MkdirAll(patternDir, 0755); err != nil {
		t.Fatal(err)
	}
	// A simple recognizable MIT text snippet for the classifier
	patternContent := []byte(`Permission is hereby granted, free of charge, to any person obtaining a copy`)
	if err := os.WriteFile(filepath.Join(patternDir, "mit.txt"), patternContent, 0644); err != nil {
		t.Fatal(err)
	}

	// Scaffold configs dir to avoid Assemble errors
	os.MkdirAll(filepath.Join(tempDir, "tools", "check-licenses", "assets", "configs"), 0755)

	// 2. Create a third_party project with a README
	projectDir := filepath.Join(tempDir, "third_party", "foo")
	if err := os.MkdirAll(projectDir, 0755); err != nil {
		t.Fatal(err)
	}
	readmeContent := []byte("Name: foo\nURL: http://foo\nVersion: 1.0\nSecurity Critical: no\nLicense File: declared.cc\n  License: MIT\n")
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
	if err := os.WriteFile(undeclaredFile, []byte("/* Permission is hereby granted, free of charge, to any person obtaining a copy */\nint helper() {}"), 0644); err != nil {
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

	patternDir := filepath.Join(tempDir, "tools", "check-licenses", "assets", "patterns", "Permissive", "MIT")
	if err := os.MkdirAll(patternDir, 0755); err != nil {
		t.Fatal(err)
	}
	patternContent := []byte(`Permission is hereby granted, free of charge, to any person obtaining a copy`)
	if err := os.WriteFile(filepath.Join(patternDir, "mit.txt"), patternContent, 0644); err != nil {
		t.Fatal(err)
	}

	os.MkdirAll(filepath.Join(tempDir, "tools", "check-licenses", "assets", "configs"), 0755)

	projectDir := filepath.Join(tempDir, "third_party", "foo")
	if err := os.MkdirAll(projectDir, 0755); err != nil {
		t.Fatal(err)
	}

	readmeContent := []byte("Name: foo\nURL: http://foo\nVersion: 1.0\nSecurity Critical: no\n\nNon-License File: ignored.cc\n  Non-License File Explanation: False positive\n")
	if err := os.WriteFile(filepath.Join(projectDir, "README.fuchsia"), readmeContent, 0644); err != nil {
		t.Fatal(err)
	}

	os.WriteFile(filepath.Join(projectDir, "ignored.cc"), []byte("/* Permission is hereby granted, free of charge, to any person obtaining a copy */\n"), 0644)
	os.WriteFile(filepath.Join(projectDir, "found.cc"), []byte("/* Permission is hereby granted, free of charge, to any person obtaining a copy */\n"), 0644)
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

	if !strings.Contains(content, "License File: found.cc") {
		t.Errorf("Expected found.cc to be added to License File list, got:\n%s", content)
	}
	if !strings.Contains(content, "License File: LICENSE") {
		t.Errorf("Expected LICENSE to be added to License File list, got:\n%s", content)
	}
	if !strings.Contains(content, "Non-License File: ignored.cc") {
		t.Errorf("Expected ignored.cc to be preserved in Non-License File list, got:\n%s", content)
	}
	if strings.Contains(content, "\nLicense File: ignored.cc") {
		t.Errorf("Expected ignored.cc to NOT be added to License File list, got:\n%s", content)
	}
}

func TestProjectCommand_ListAndInfo(t *testing.T) {
	tempDir := t.TempDir()

	os.MkdirAll(filepath.Join(tempDir, "tools", "check-licenses", "assets", "configs"), 0755)

	patternDir := filepath.Join(tempDir, "tools", "check-licenses", "assets", "patterns", "Permissive", "MIT")
	os.MkdirAll(patternDir, 0755)
	os.WriteFile(filepath.Join(patternDir, "mit.txt"), []byte("Permission is hereby granted"), 0644)

	projectDir := filepath.Join(tempDir, "third_party", "foo")
	if err := os.MkdirAll(projectDir, 0755); err != nil {
		t.Fatal(err)
	}
	readmeContent := []byte("Name: foo\nURL: http://foo\nVersion: 1.0\nSecurity Critical: no\nLicense File: declared.cc\n  License: MIT\n")
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
