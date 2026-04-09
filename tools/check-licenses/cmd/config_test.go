// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"os"
	"path/filepath"
	"testing"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/directory"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/file"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/project"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/result"
)

// =========================================================================
// JSON Parsing and Variable Expansion Tests
// =========================================================================

// TestConfig_VariableExpansion verifies that variables like {MY_VAR} are replaced correctly.
func TestConfig_VariableExpansion(t *testing.T) {
	configJson := `{"fuchsiaDir": "{MY_VAR}/src"}`
	configVars := map[string]string{
		"{MY_VAR}": "/opt/fuchsia",
	}

	c, err := NewCheckLicensesConfigJson(configJson, configVars)
	if err != nil {
		t.Fatalf("Failed to parse JSON: %v", err)
	}

	if c.FuchsiaDir != "/opt/fuchsia/src" {
		t.Errorf("Expected FuchsiaDir to be '/opt/fuchsia/src', got %q", c.FuchsiaDir)
	}
}

// TestConfig_UnexpandedVariable verifies that missing variables cause an error.
func TestConfig_UnexpandedVariable(t *testing.T) {
	configJson := `{"fuchsiaDir": "{UNKNOWN_VAR}/src"}`
	configVars := map[string]string{} // empty

	_, err := NewCheckLicensesConfigJson(configJson, configVars)
	if err == nil {
		t.Fatal("Expected error for unexpanded variable, got nil")
	}
}

// =========================================================================
// Merge Logic Tests
// =========================================================================

// TestConfig_Merge verifies that Merge combines configs correctly without overwriting
// already set scalar fields, whilst concatenating slices and taking max of integers.
func TestConfig_Merge(t *testing.T) {
	c1 := &CheckLicensesConfig{
		FuchsiaDir: "/original",
		Target:     "",
		LogLevel:   1,
		Includes:   []Include{{Path: []string{"a"}}},
		File:       file.NewConfig(),
		Project:    project.NewConfig(),
		Directory:  directory.NewConfig(),
		Result:     result.NewConfig(),
	}

	c2 := &CheckLicensesConfig{
		FuchsiaDir: "/new",
		Target:     "//:foo",
		LogLevel:   2,
		Includes:   []Include{{Path: []string{"b"}}},
		File:       file.NewConfig(),
		Project:    project.NewConfig(),
		Directory:  directory.NewConfig(),
		Result:     result.NewConfig(),
	}

	c1.Merge(c2)

	if c1.FuchsiaDir != "/original" {
		t.Errorf("Expected FuchsiaDir to not be overwritten, got %q", c1.FuchsiaDir)
	}
	if c1.Target != "//:foo" {
		t.Errorf("Expected Target to be populated, got %q", c1.Target)
	}
	if c1.LogLevel != 2 {
		t.Errorf("Expected LogLevel to take max, got %d", c1.LogLevel)
	}
	if len(c1.Includes) != 2 || c1.Includes[1].Path[0] != "b" {
		t.Errorf("Expected Includes to be appended")
	}
}

// =========================================================================
// Inclusion Engine Tests (ProcessIncludes)
// =========================================================================

// TestProcessIncludes_NonRecursive verifies direct file inclusion.
func TestProcessIncludes_NonRecursive(t *testing.T) {
	tempDir := t.TempDir()

	includePath := filepath.Join(tempDir, "include.json")
	includeContent := `{"outDir": "from_include"}`
	os.WriteFile(includePath, []byte(includeContent), 0644)

	masterConfig := &CheckLicensesConfig{
		Includes: []Include{
			{Path: []string{includePath}, Recursive: false},
		},
		File:      file.NewConfig(),
		Project:   project.NewConfig(),
		Directory: directory.NewConfig(),
		Result:    result.NewConfig(),
	}

	configVars := map[string]string{}
	err := masterConfig.ProcessIncludes(configVars)
	if err != nil {
		t.Fatalf("ProcessIncludes failed: %v", err)
	}

	if masterConfig.OutDir != "from_include" {
		t.Errorf("Expected outDir 'from_include', got %q", masterConfig.OutDir)
	}
}

// TestProcessIncludes_Recursive verifies recursive folder inclusion and validates
// the bugfix where non-JSON files (like c.txt) are safely skipped without panicking.
func TestProcessIncludes_Recursive(t *testing.T) {
	tempDir := t.TempDir()
	incDir := filepath.Join(tempDir, "includes")
	os.MkdirAll(incDir, 0755)

	os.WriteFile(filepath.Join(incDir, "a.json"), []byte(`{"target": "A"}`), 0644)
	os.WriteFile(filepath.Join(incDir, "b.json"), []byte(`{"outDir": "B"}`), 0644)
	os.WriteFile(filepath.Join(incDir, "c.txt"), []byte(`invalid json garbage`), 0644)

	masterConfig := &CheckLicensesConfig{
		Includes: []Include{
			{Path: []string{incDir}, Recursive: true},
		},
		File:      file.NewConfig(),
		Project:   project.NewConfig(),
		Directory: directory.NewConfig(),
		Result:    result.NewConfig(),
	}

	configVars := map[string]string{}
	err := masterConfig.ProcessIncludes(configVars)
	if err != nil {
		t.Fatalf("ProcessIncludes failed: %v", err)
	}

	// Because of how Walk works, the order of merging A vs B is technically non-deterministic,
	// but for testing independent fields this is fine.
	if masterConfig.Target != "A" {
		t.Errorf("Expected Target 'A', got %q", masterConfig.Target)
	}
	if masterConfig.OutDir != "B" {
		t.Errorf("Expected outDir 'B', got %q", masterConfig.OutDir)
	}
}

// TestProcessIncludes_RequiredFlag verifies that the Required boolean strictly
// controls whether os.ErrNotExist is fatal or gracefully swallowed.
func TestProcessIncludes_RequiredFlag(t *testing.T) {
	masterConfig := &CheckLicensesConfig{
		Includes: []Include{
			{Path: []string{"/does/not/exist.json"}, Required: true},
		},
		File:      file.NewConfig(),
		Project:   project.NewConfig(),
		Directory: directory.NewConfig(),
		Result:    result.NewConfig(),
	}

	err := masterConfig.ProcessIncludes(map[string]string{})
	if err == nil {
		t.Fatal("Expected error for required missing include, got nil")
	}

	masterConfig.Includes[0].Required = false
	err = masterConfig.ProcessIncludes(map[string]string{})
	if err != nil {
		t.Fatalf("Expected NO error for non-required missing include, got %v", err)
	}
}
