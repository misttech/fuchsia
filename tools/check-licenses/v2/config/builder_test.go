// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package config

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

func TestBuilder_Assemble(t *testing.T) {
	// Create a mock fuchsia directory
	fuchsiaDir := t.TempDir()

	// Scaffold the recursive config system
	seedConfig := filepath.Join(fuchsiaDir, "tools", "check-licenses", "v2", "config.json")
	os.MkdirAll(filepath.Dir(seedConfig), 0755)
	os.WriteFile(seedConfig, []byte(`{"includes": ["tools/check-licenses/assets", "vendor/google/tools/check-licenses/assets"]}`), 0644)

	// 1. Setup mock open-source assets
	osAssets := filepath.Join(fuchsiaDir, "tools", "check-licenses", "assets")
	osConfigs := filepath.Join(osAssets, "configs")
	if err := os.MkdirAll(filepath.Join(osConfigs, "skips"), 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(filepath.Join(osConfigs, "target_extensions"), 0755); err != nil {
		t.Fatal(err)
	}

	osSkipBytes, _ := json.Marshal(ConfigFile{
		Skips: []SkipEntry{
			{
				Bug:   "https://fxbug.dev/12345",
				Paths: []string{"out", "prebuilt"},
			},
		},
	})
	os.WriteFile(filepath.Join(osConfigs, "skips", "test_skip.json"), osSkipBytes, 0644)

	osExtBytes, _ := json.Marshal(ConfigFile{
		TargetExtensions: &ExtensionEntry{
			Extensions: []string{".cc", "rs"}, // intentionally missing dot on rs
		},
	})
	os.WriteFile(filepath.Join(osConfigs, "target_extensions", "test_ext.json"), osExtBytes, 0644)

	if err := os.MkdirAll(filepath.Join(osAssets, "readmes", "third_party", "foo", "src"), 0755); err != nil {
		t.Fatal(err)
	}
	readmePath := filepath.Join(osAssets, "readmes", "third_party", "foo", "README.fuchsia")
	os.WriteFile(readmePath, []byte("Name: Foo"), 0644)

	// 2. Setup mock proprietary vendor assets
	vendorAssets := filepath.Join(fuchsiaDir, "vendor", "google", "tools", "check-licenses", "assets")
	vendorConfigs := filepath.Join(vendorAssets, "configs")
	if err := os.MkdirAll(filepath.Join(vendorConfigs, "projects"), 0755); err != nil {
		t.Fatal(err)
	}
	vendorAllowBytes, _ := json.Marshal(ConfigFile{
		PolicyExceptions: map[string][]AllowlistEntry{
			"AllProjectsMustHaveALicense": {
				{
					Paths: []string{"vendor/google/secret_project"},
				},
			},
		},
	})
	// Test the "default.json" exception allows missing bug field
	os.WriteFile(filepath.Join(vendorConfigs, "projects", "default.json"), vendorAllowBytes, 0644)

	// 3. Run the Builder
	builder := NewBuilder(fuchsiaDir)
	if err := builder.Assemble(); err != nil {
		t.Fatalf("Assemble failed: %v", err)
	}

	config := builder.Config

	// 4. Verify results
	expectedSkips := []string{"out", "prebuilt"}
	if !reflect.DeepEqual(config.SkipPaths, expectedSkips) {
		t.Errorf("Expected skips %v, got %v", expectedSkips, config.SkipPaths)
	}

	expectedExts := map[string]bool{".cc": true, ".rs": true}
	if !reflect.DeepEqual(config.TargetExtensions, expectedExts) {
		t.Errorf("Expected extensions %v, got %v", expectedExts, config.TargetExtensions)
	}

	logicalPath := filepath.Join("third_party", "foo")
	if config.OutOfTreeReadmes[logicalPath] != readmePath {
		t.Errorf("Expected OutOfTreeReadmes[%q] = %q, got %q", logicalPath, readmePath, config.OutOfTreeReadmes[logicalPath])
	}

	if _, ok := config.PolicyExceptions["AllProjectsMustHaveALicense"]["vendor/google/secret_project"]; !ok {
		t.Errorf("Expected vendor project to be in the policy exceptions list")
	}
}

func TestBuilder_LoadManifests(t *testing.T) {
	fuchsiaDir := t.TempDir()

	// Create mock manifests directory
	manifestsDir := filepath.Join(fuchsiaDir, "manifests")
	if err := os.MkdirAll(manifestsDir, 0755); err != nil {
		t.Fatal(err)
	}

	// Create a mock public manifest
	publicManifest := filepath.Join(manifestsDir, "public_projects")
	publicContent := `<?xml version="1.0" encoding="UTF-8"?>
<manifest>
  <project name="third_party/acpica" path="third_party/acpica"/>
  <packages>
    <package name="fuchsia/third_party/clang" path="prebuilt/third_party/clang"/>
    <package name="fuchsia_internal/firmware/amlogic-video" path="prebuilt/media/firmware/amlogic-decoder"/>
  </packages>
</manifest>`
	os.WriteFile(publicManifest, []byte(publicContent), 0644)

	// Create mock integration directory
	integrationDir := filepath.Join(fuchsiaDir, "integration", "internal", "vendor", "google")
	if err := os.MkdirAll(integrationDir, 0755); err != nil {
		t.Fatal(err)
	}

	// Create a mock private manifest
	privateManifest := filepath.Join(integrationDir, "third_party")
	privateContent := `<?xml version="1.0" encoding="UTF-8"?>
<manifest>
  <project name="eigen/fuchsia" path="vendor/third_party/eigen3"/>
</manifest>`
	os.WriteFile(privateManifest, []byte(privateContent), 0644)

	builder := NewBuilder(fuchsiaDir)
	if err := builder.LoadManifests(); err != nil {
		t.Fatalf("LoadManifests failed: %v", err)
	}

	config := builder.Config

	// Verify mappings
	expectedMappings := map[string]string{
		filepath.Clean("third_party/acpica"):                      "third_party/acpica",
		filepath.Clean("prebuilt/third_party/clang"):              "fuchsia/third_party/clang",
		filepath.Clean("prebuilt/media/firmware/amlogic-decoder"): "fuchsia_internal/firmware/amlogic-video",
		filepath.Clean("vendor/third_party/eigen3"):               "eigen/fuchsia",
	}

	for path, expectedName := range expectedMappings {
		if name, ok := config.ManifestProjectNames[path]; !ok || name != expectedName {
			t.Errorf("Expected ManifestProjectNames[%q] = %q, got %q", path, expectedName, name)
		}
	}

	// Verify IsPrivateProject
	tests := []struct {
		path      string
		isPrivate bool
	}{
		{"third_party/acpica", false},
		{"prebuilt/third_party/clang", false},
		{"prebuilt/media/firmware/amlogic-decoder", true},
		{"vendor/third_party/eigen3", true},
		{"unknown/project", false},
	}

	for _, tc := range tests {
		if got := config.IsPrivateProject(tc.path); got != tc.isPrivate {
			t.Errorf("IsPrivateProject(%q) = %v, want %v", tc.path, got, tc.isPrivate)
		}
	}
}
