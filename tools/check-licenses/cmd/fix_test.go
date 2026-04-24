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

	"github.com/google/subcommands"
)

func TestFixCommand_Execute(t *testing.T) {
	tempDir := t.TempDir()

	// 1. Scaffold the recursive config system
	seedConfig := filepath.Join(tempDir, "tools", "check-licenses", "v2", "config.json")
	os.MkdirAll(filepath.Dir(seedConfig), 0755)
	os.WriteFile(seedConfig, []byte(`{"includes": ["tools/check-licenses/assets"]}`), 0644)

	// Scaffold necessary assets for the v2 pipeline to run
	// Patterns for detection
	patternsBase := filepath.Join(tempDir, "tools", "check-licenses", "assets", "patterns")
	copyrightPatternDir := filepath.Join(patternsBase, "_Header", "FuchsiaCopyright")
	os.MkdirAll(copyrightPatternDir, 0755)
	os.WriteFile(filepath.Join(copyrightPatternDir, "fuchsia.txt"), []byte("Copyright 2026 The Fuchsia Authors. All rights reserved."), 0644)

	mitPatternDir := filepath.Join(patternsBase, "Permissive", "MIT")
	os.MkdirAll(mitPatternDir, 0755)
	os.WriteFile(filepath.Join(mitPatternDir, "mit.txt"), []byte("MIT License"), 0644)

	gplPatternDir := filepath.Join(patternsBase, "Restricted", "GPL-2.0")
	os.MkdirAll(gplPatternDir, 0755)
	os.WriteFile(filepath.Join(gplPatternDir, "gpl.txt"), []byte("GPL License"), 0644)

	// Configs for category discovery
	gplConfigDir := filepath.Join(tempDir, "tools", "check-licenses", "assets", "configs", "allowed_licenses", "Restricted", "GPL-2.0")
	os.MkdirAll(gplConfigDir, 0755)
	os.WriteFile(filepath.Join(gplConfigDir, "default.json"), []byte("{}"), 0644)

	// 2. Create a project with multiple issues
	// 1st project: Root (First-party)
	// Issue 1: Missing Copyright Header
	sourceFile := filepath.Join(tempDir, "main.cc")
	os.WriteFile(sourceFile, []byte("void main() {\n}\n"), 0644)

	// Issue 2: Out of date README (missing the LICENSE file we're about to add)
	readmePath := filepath.Join(tempDir, "README.fuchsia")
	os.WriteFile(readmePath, []byte("Name: fuchsia\nURL: http://fuchsia.dev\nVersion: 1.0\nSecurity Critical: yes\n"), 0644)

	licenseFile := filepath.Join(tempDir, "LICENSE")
	os.WriteFile(licenseFile, []byte("MIT License"), 0644)

	// 2nd project: vendor/testproj (Third-party)
	vendorDir := filepath.Join(tempDir, "vendor", "testproj")
	os.MkdirAll(vendorDir, 0755)
	os.WriteFile(filepath.Join(vendorDir, "README.fuchsia"), []byte("Name: testproj\nURL: http://test.com\nVersion: 1.0\nSecurity Critical: no\nLicense File: LICENSE\n"), 0644)
	os.WriteFile(filepath.Join(vendorDir, "LICENSE"), []byte("GPL License"), 0644)

	// Create mock private manifest for vendor/testproj
	integrationDir := filepath.Join(tempDir, "integration", "internal", "vendor", "google")
	if err := os.MkdirAll(integrationDir, 0755); err != nil {
		t.Fatal(err)
	}
	privateManifest := filepath.Join(integrationDir, "third_party")
	privateContent := `<?xml version="1.0" encoding="UTF-8"?>
<manifest>
  <project name="testproj" path="vendor/testproj"/>
</manifest>`
	os.WriteFile(privateManifest, []byte(privateContent), 0644)

	// 3. Initialize and run the FixCommand on tempDir
	cmd := &FixCommand{
		fuchsiaDir: tempDir,
	}
	ctx := context.Background()
	fs := flag.NewFlagSet("test", flag.ContinueOnError)
	fs.Parse([]string{tempDir})

	status := cmd.Execute(ctx, fs)
	if status != subcommands.ExitSuccess {
		t.Fatalf("Expected fix command to exit with success, got %v", status)
	}

	// 4. Verify fixes

	// Verify Fix 1: Copyright header added to root file
	content, _ := os.ReadFile(sourceFile)
	if !strings.Contains(string(content), "The Fuchsia Authors") {
		t.Errorf("Expected copyright header to be added to %s. Content:\n%s", sourceFile, string(content))
	}

	// Verify Fix 2: README updated with license info
	readmeContent, _ := os.ReadFile(readmePath)
	if !strings.Contains(string(readmeContent), "License File: LICENSE") {
		t.Errorf("Expected README to be updated with license file info. Content:\n%s", string(readmeContent))
	}

	// Verify Fix 3: Allowlist entry created for GPL-2.0 in vendor project
	allowlistPath := filepath.Join(tempDir, "vendor", "google", "tools", "check-licenses", "assets", "configs", "allowed_licenses", "Restricted", "GPL-2.0", "testproj.json")
	if _, err := os.Stat(allowlistPath); os.IsNotExist(err) {
		t.Errorf("Expected allowlist config file to be created at %s", allowlistPath)
	}
}
