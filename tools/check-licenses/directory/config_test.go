// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package directory

import (
	"testing"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/project"
)

// =========================================================================
// Configuration and Initialization Tests
// =========================================================================

// TestDirectoryConfig_Merge verifies that two DirectoryConfigs can be properly
// merged without dropping paths or incorrectly overriding the FuchsiaDir.
func TestDirectoryConfig_Merge(t *testing.T) {
	c1 := NewConfig()
	c1.FuchsiaDir = "/original/dir"
	c1.Skips = []*Skip{{Paths: []string{"path1"}}}

	c2 := NewConfig()
	c2.FuchsiaDir = "/new/dir"
	c2.Skips = []*Skip{{Paths: []string{"path2"}}}

	c1.Merge(c2)

	if c1.FuchsiaDir != "/original/dir" {
		t.Errorf("Expected FuchsiaDir to NOT be overridden if already set, got %q", c1.FuchsiaDir)
	}

	if len(c1.Skips) != 2 {
		t.Fatalf("Expected 2 skips after merge, got %d", len(c1.Skips))
	}
	if c1.Skips[0].Paths[0] != "path1" || c1.Skips[1].Paths[0] != "path2" {
		t.Error("Expected paths to be appended properly")
	}
}

// TestInitialize verifies that the global Initialize function properly sets up
// the default skip paths from the project boundaries and pattern configurations.
func TestInitialize(t *testing.T) {
	setup(t)
	c := NewConfig()

	// Mock a project readme configuration
	project.Config.Readmes = []*project.Readme{
		{Paths: []string{"mock/readme/path"}},
	}

	if err := Initialize(c); err != nil {
		t.Fatal(err)
	}

	if Config != c {
		t.Error("Expected global Config to be set to the initialized config")
	}

	// It should have appended the Project Readme skips and the Pattern skips
	if len(c.Skips) != 2 {
		t.Fatalf("Expected exactly 2 skip categories injected by Initialize, got %d", len(c.Skips))
	}

	readmeSkips := c.Skips[0]
	if len(readmeSkips.Paths) != 1 || readmeSkips.Paths[0] != "mock/readme/path" {
		t.Errorf("Expected Readme paths to be injected into Skips")
	}
}
