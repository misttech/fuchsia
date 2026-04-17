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
	"time"

	"github.com/google/subcommands"
)

func TestClassifyCommand_Execute(t *testing.T) {
	tempDir := t.TempDir()

	// Scaffold the fake assets/patterns directory
	patternDir := filepath.Join(tempDir, "tools", "check-licenses", "assets", "patterns", "Permissive", "MIT")
	if err := os.MkdirAll(patternDir, 0755); err != nil {
		t.Fatal(err)
	}
	patternContent := []byte(`Permission is hereby granted, free of charge, to any person obtaining a copy`)
	if err := os.WriteFile(filepath.Join(patternDir, "mit.txt"), patternContent, 0644); err != nil {
		t.Fatal(err)
	}

	testFilePath := filepath.Join(tempDir, "test_license.txt")
	content := []byte("Some random text.\nPermission is hereby granted, free of charge, to any person obtaining a copy\nMore random text.")
	if err := os.WriteFile(testFilePath, content, 0644); err != nil {
		t.Fatal(err)
	}

	cmd := &ClassifyCommand{
		fuchsiaDir: tempDir,
	}

	fs := flag.NewFlagSet("test", flag.ContinueOnError)
	cmd.SetFlags(fs)
	fs.Parse([]string{"--fuchsia_dir", tempDir, testFilePath})

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	status := cmd.Execute(ctx, fs)
	if status != subcommands.ExitSuccess {
		t.Errorf("Expected ExitSuccess (0) for classifying file, got %v", status)
	}
}
