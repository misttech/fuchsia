// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package result

import (
	"os"
	"path/filepath"
	"testing"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/file"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/project"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/readme"
)

// =========================================================================
// World Context Builder Tests (world.go)
// =========================================================================

func TestGetWorldStruct_Deduplication(t *testing.T) {
	tempDir := resetState(t)

	// We are explicitly testing the deduplication algorithm here.
	// We'll create three projects. Project A and B share the exact same license text.
	// Project C has a different text.

	pA := &project.Project{Root: "src/A", Name: "ProjectA", ReadmeFile: &readme.Readme{}}
	pB := &project.Project{Root: "src/B", Name: "ProjectB", ReadmeFile: &readme.Readme{}}
	pC := &project.Project{Root: "src/C", Name: "ProjectC", ReadmeFile: &readme.Readme{}}

	project.AddFilteredProject(pA)
	project.AddFilteredProject(pB)
	project.AddFilteredProject(pC)

	// Create identical license texts for A and B in DIFFERENT files to avoid the file package cache returning ProjectA's object.
	pathSharedA := filepath.Join(tempDir, "LICENSE.A")
	pathSharedB := filepath.Join(tempDir, "LICENSE.B")
	os.WriteFile(pathSharedA, []byte("Shared MIT License"), 0644)
	os.WriteFile(pathSharedB, []byte("Shared MIT License"), 0644)

	fA, _ := file.LoadFile(pathSharedA, file.SingleLicense, "ProjectA")
	pA.AddFile(fA)

	fB, _ := file.LoadFile(pathSharedB, file.SingleLicense, "ProjectB")
	pB.AddFile(fB)

	// Create different text for C
	pathUnique := filepath.Join(tempDir, "LICENSE.unique")
	os.WriteFile(pathUnique, []byte("Unique Apache License"), 0644)

	fC, _ := file.LoadFile(pathUnique, file.SingleLicense, "ProjectC")
	pC.AddFile(fC)

	// Force initialization of data segments so hashes are generated
	fA.Data()
	fB.Data()
	fC.Data()

	w := getWorldStruct()

	if len(w.DedupedLicenseData) != 2 {
		t.Fatalf("Expected exactly 2 deduplicated license groups, got %d", len(w.DedupedLicenseData))
	}

	var sharedGroup, uniqueGroup *DedupedLicense
	for _, group := range w.DedupedLicenseData {
		if group.Text == "Shared MIT License" {
			sharedGroup = group
		} else if group.Text == "Unique Apache License" {
			uniqueGroup = group
		}
	}

	if sharedGroup == nil || uniqueGroup == nil {
		t.Fatal("Failed to find both the shared and unique license groups")
	}

	if len(sharedGroup.LibraryNames) != 2 {
		t.Errorf("Expected 2 libraries in the shared group, got %d", len(sharedGroup.LibraryNames))
	}
	if sharedGroup.LibraryNames[0] != "ProjectA" || sharedGroup.LibraryNames[1] != "ProjectB" {
		t.Errorf("Expected LibraryNames to be ProjectA and ProjectB (sorted), got %v", sharedGroup.LibraryNames)
	}

	if len(uniqueGroup.LibraryNames) != 1 || uniqueGroup.LibraryNames[0] != "ProjectC" {
		t.Errorf("Expected 1 library in the unique group (ProjectC), got %v", uniqueGroup.LibraryNames)
	}
}
