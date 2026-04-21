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

	publicConfigPath := filepath.Join(tempDir, "tools", "check-licenses", "assets", "configs", "policy_exceptions", "AllProjectsMustHaveALicense", "bar.json")
	if _, err := os.Stat(publicConfigPath); os.IsNotExist(err) {
		t.Errorf("Expected config file to be created at %s", publicConfigPath)
	}

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
}
