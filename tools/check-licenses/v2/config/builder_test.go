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
