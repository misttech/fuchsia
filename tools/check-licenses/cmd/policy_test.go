// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"context"
	"flag"
	"os"
	"path/filepath"
	"testing"

	"github.com/google/subcommands"
)

func TestPolicyCommand_Execute(t *testing.T) {
	tempDir := t.TempDir()

	origWd, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	defer os.Chdir(origWd)
	if err := os.Chdir(tempDir); err != nil {
		t.Fatal(err)
	}

	// Scaffold the recursive config system
	seedConfig := filepath.Join(tempDir, "tools", "check-licenses", "v2", "config.json")
	os.MkdirAll(filepath.Dir(seedConfig), 0755)
	os.WriteFile(seedConfig, []byte(`{"includes": ["tools/check-licenses/assets"]}`), 0644)

	// Create command
	cmd := &PolicyCommand{
		fuchsiaDir: tempDir,
	}

	ctx := context.Background()

	// Test 1: Missing arguments
	f1 := flag.NewFlagSet("test1", flag.ContinueOnError)
	f1.Parse([]string{})
	if status := cmd.Execute(ctx, f1); status != subcommands.ExitUsageError {
		t.Errorf("Expected ExitUsageError for missing args, got %v", status)
	}

	// Test 2: Public project
	f2 := flag.NewFlagSet("test2", flag.ContinueOnError)
	f2.Parse([]string{"add", "AllProjectsMustHaveALicense", "src/foo/bar"})
	if status := cmd.Execute(ctx, f2); status != subcommands.ExitSuccess {
		t.Errorf("Expected ExitSuccess for public project, got %v", status)
	}

	publicConfigPath := filepath.Join(tempDir, "tools", "check-licenses", "assets", "configs", "policy_exceptions", "AllProjectsMustHaveALicense", "foo.json")
	if _, err := os.Stat(publicConfigPath); os.IsNotExist(err) {
		t.Errorf("Expected config file to be created at %s", publicConfigPath)
	}

	// Create mock private manifest for Test 3
	integrationDir := filepath.Join(tempDir, "integration", "internal", "vendor", "google")
	if err := os.MkdirAll(integrationDir, 0755); err != nil {
		t.Fatal(err)
	}
	privateManifest := filepath.Join(integrationDir, "third_party")
	privateContent := `<?xml version="1.0" encoding="UTF-8"?>
<manifest>
  <project name="my_private_proj" path="vendor/my_private_proj"/>
</manifest>`
	os.WriteFile(privateManifest, []byte(privateContent), 0644)

	// Test 3: Private project (vendor/...)
	f3 := flag.NewFlagSet("test3", flag.ContinueOnError)
	f3.Parse([]string{"add", "AllProjectsMustHaveALicense", "vendor/my_private_proj"})
	if status := cmd.Execute(ctx, f3); status != subcommands.ExitSuccess {
		t.Errorf("Expected ExitSuccess for private project, got %v", status)
	}

	privateConfigPath := filepath.Join(tempDir, "vendor", "google", "tools", "check-licenses", "assets", "configs", "policy_exceptions", "AllProjectsMustHaveALicense", "my_private_proj.json")
	if _, err := os.Stat(privateConfigPath); os.IsNotExist(err) {
		t.Errorf("Expected config file to be created at %s", privateConfigPath)
	}

	// Test 4: Already exists (should not fail, should exit success)
	f4 := flag.NewFlagSet("test4", flag.ContinueOnError)
	f4.Parse([]string{"add", "AllProjectsMustHaveALicense", "src/foo/bar"})
	if status := cmd.Execute(ctx, f4); status != subcommands.ExitSuccess {
		t.Errorf("Expected ExitSuccess when exception already exists, got %v", status)
	}

	// Test 5: Third party file grouping (should group by project name from manifest)
	f5 := flag.NewFlagSet("test5", flag.ContinueOnError)
	f5.Parse([]string{"add", "AllLicenseTextsMustBeRecognized", "vendor/my_private_proj/LICENSE"})
	if status := cmd.Execute(ctx, f5); status != subcommands.ExitSuccess {
		t.Errorf("Expected ExitSuccess for third party file, got %v", status)
	}

	thirdPartyConfigPath := filepath.Join(tempDir, "vendor", "google", "tools", "check-licenses", "assets", "configs", "policy_exceptions", "AllLicenseTextsMustBeRecognized", "my_private_proj.json")
	if _, err := os.Stat(thirdPartyConfigPath); os.IsNotExist(err) {
		t.Errorf("Expected config file to be created at %s", thirdPartyConfigPath)
	}
}

func TestPolicyCommand_Execute_AssemblyFailure(t *testing.T) {
	tempDir := t.TempDir()

	origWd, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	defer os.Chdir(origWd)
	if err := os.Chdir(tempDir); err != nil {
		t.Fatal(err)
	}

	// Scaffold a corrupt config file
	seedConfig := filepath.Join(tempDir, "tools", "check-licenses", "v2", "config.json")
	os.MkdirAll(filepath.Dir(seedConfig), 0755)
	os.WriteFile(seedConfig, []byte(`{corrupt json}`), 0644)

	cmd := &PolicyCommand{
		fuchsiaDir: tempDir,
	}

	ctx := context.Background()
	f := flag.NewFlagSet("test_failure", flag.ContinueOnError)
	f.Parse([]string{"add", "AllProjectsMustHaveALicense", "src/foo/bar"})

	if status := cmd.Execute(ctx, f); status != subcommands.ExitFailure {
		t.Errorf("Expected ExitFailure for assembly error, got %v", status)
	}
}

func TestPolicyCommand_Execute_RelativePathFromSubdir(t *testing.T) {
	tempDir := t.TempDir()

	// Scaffold the recursive config system
	seedConfig := filepath.Join(tempDir, "tools", "check-licenses", "v2", "config.json")
	os.MkdirAll(filepath.Dir(seedConfig), 0755)
	os.WriteFile(seedConfig, []byte(`{"includes": ["tools/check-licenses/assets"]}`), 0644)

	cmd := &PolicyCommand{
		fuchsiaDir: tempDir,
	}

	ctx := context.Background()

	// Simulate running from a subdirectory
	origWd, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	defer os.Chdir(origWd)

	subdir := filepath.Join(tempDir, "src", "my_project")
	if err := os.MkdirAll(subdir, 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.Chdir(subdir); err != nil {
		t.Fatal(err)
	}

	f := flag.NewFlagSet("test_relative", flag.ContinueOnError)
	f.Parse([]string{"add", "AllProjectsMustHaveALicense", "."}) // target is "." (src/my_project)

	if status := cmd.Execute(ctx, f); status != subcommands.ExitSuccess {
		t.Errorf("Expected ExitSuccess, got %v", status)
	}

	// Expected config file should be named "my_project.json" under the check name directory
	expectedConfigPath := filepath.Join(tempDir, "tools", "check-licenses", "assets", "configs", "policy_exceptions", "AllProjectsMustHaveALicense", "my_project.json")
	if _, err := os.Stat(expectedConfigPath); os.IsNotExist(err) {
		t.Errorf("Expected config file to be created at %s", expectedConfigPath)
	}
}
