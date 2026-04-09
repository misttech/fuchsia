// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package project

import (
	"os"
	"path/filepath"
	"sort"
	"sync"
	"testing"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/file"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/readme"
)

// setup initializes the global state across packages to ensure isolation between tests.
func setup(t *testing.T) string {
	t.Helper()
	tempDir := t.TempDir()

	file.Config = file.NewConfig()
	file.Config.FuchsiaDir = tempDir
	// Initialize a searchable extension for testing
	file.Config.Extensions[".rs"] = true

	Config = NewConfig()
	Config.FuchsiaDir = tempDir

	InitializeForTest()
	readme.InitializeForTest()

	return tempDir
}

// =========================================================================
// NewProject Tests
// =========================================================================

// TestNewProject_Success validates the core constructor logic.
// It verifies that a valid Readme and path result in a correctly populated Project
// struct that is successfully cached in the global allProjects map.
func TestNewProject_Success(t *testing.T) {
	setup(t)

	r := &readme.Readme{
		Name:     "TestProject",
		URL:      "http://example.com",
		Licenses: make([]*readme.ReadmeLicense, 0),
	}

	p, err := NewProject(r, "src/test_project")
	if err != nil {
		t.Fatal(err)
	}

	if p.Name != "TestProject" || p.URL != "http://example.com" || p.Root != "src/test_project" {
		t.Errorf("Project fields incorrectly populated: %+v", p)
	}
	if p.ReadmeFile != r {
		t.Error("Project ReadmeFile pointer incorrectly assigned")
	}

	cached, ok := GetProject("src/test_project")
	if !ok || cached != p {
		t.Error("Project was not successfully registered in the global cache")
	}
}

// TestNewProject_PathNormalization verifies that absolute paths passed to
// NewProject are correctly stripped of the FuchsiaDir prefix to ensure all
// roots are stored relatively.
func TestNewProject_PathNormalization(t *testing.T) {
	tempDir := setup(t)

	r := &readme.Readme{Licenses: make([]*readme.ReadmeLicense, 0)}
	absPath := filepath.Join(tempDir, "src", "normalized")

	p, err := NewProject(r, absPath)
	if err != nil {
		t.Fatal(err)
	}

	expectedRel := filepath.Join("src", "normalized")
	if p.Root != expectedRel {
		t.Errorf("Expected root path to be normalized to relative %q, got %q", expectedRel, p.Root)
	}
}

// TestNewProject_CacheHit verifies that invoking NewProject twice for the same
// path correctly short-circuits and returns the exact same Project pointer.
func TestNewProject_CacheHit(t *testing.T) {
	setup(t)

	r1 := &readme.Readme{Name: "Original"}
	r2 := &readme.Readme{Name: "Duplicate"}

	p1, err := NewProject(r1, "src/cached")
	if err != nil {
		t.Fatal(err)
	}

	p2, err := NewProject(r2, "src/cached")
	if err != nil {
		t.Fatal(err)
	}

	if p1 != p2 {
		t.Error("Expected NewProject to return the exact same pointer on cache hit")
	}
	if p2.Name != "Original" {
		t.Errorf("Expected cached project to retain original name, got %q", p2.Name)
	}
}

// TestNewProject_LicenseLoading verifies that if a README explicitly lists a license file,
// NewProject correctly loads the physical file from disk and attaches it to the project.
func TestNewProject_LicenseLoading(t *testing.T) {
	tempDir := setup(t)
	root := filepath.Join(tempDir, "src", "licensed")
	os.MkdirAll(root, 0755)

	licensePath := filepath.Join(root, "LICENSE.txt")
	os.WriteFile(licensePath, []byte("MIT License"), 0644)

	r := &readme.Readme{
		Name: "LicensedProject",
		Licenses: []*readme.ReadmeLicense{
			{LicenseFile: "LICENSE.txt"},
		},
	}

	p, err := NewProject(r, root)
	if err != nil {
		t.Fatal(err)
	}

	if len(p.LicenseFiles) != 1 {
		t.Fatalf("Expected 1 license file loaded, got %d", len(p.LicenseFiles))
	}
	if p.LicenseFiles[0].Name() != "LICENSE.txt" {
		t.Errorf("Expected license file to be LICENSE.txt, got %q", p.LicenseFiles[0].Name())
	}
	if r.Licenses[0].LicenseFileRef == nil {
		t.Error("Expected ReadmeLicense to be updated with the loaded file reference")
	}
}

// TestNewProject_LicenseLoadingFailure verifies that missing license files
// trigger a fatal error, EXCEPT when the project is an "Asset Dir" readme,
// which is designed to gracefully swallow errors to prevent roll breakages.
func TestNewProject_LicenseLoadingFailure(t *testing.T) {
	setup(t)

	r := &readme.Readme{
		Name: "BrokenProject",
		Licenses: []*readme.ReadmeLicense{
			{LicenseFile: "MISSING_FILE.txt"},
		},
	}

	_, err := NewProject(r, "src/broken")
	if err == nil {
		t.Fatal("Expected error when license file is missing from disk")
	}

	// Edge Case: Asset Dir Readmes gracefully swallow the error.
	r.IsAssetDirReadme = true
	p, err := NewProject(r, "src/broken_asset")
	if err != nil {
		t.Fatalf("Expected IsAssetDirReadme to swallow missing file error, got: %v", err)
	}
	if p == nil {
		t.Error("Expected project to be created successfully despite missing file")
	}
}

// =========================================================================
// AddFile Tests
// =========================================================================

// TestAddFile_RegularFile verifies that standard source files are appropriately
// bucketed into the RegularFiles slice (and SearchableRegularFiles if they match Config.Extensions).
func TestAddFile_RegularFile(t *testing.T) {
	tempDir := setup(t)
	p, _ := NewProject(&readme.Readme{}, "src/files")

	// Create a standard searchable file (e.g. .rs)
	path1 := filepath.Join(tempDir, "src/files/main.rs")
	os.MkdirAll(filepath.Dir(path1), 0755)
	os.WriteFile(path1, []byte("code"), 0644)
	fSearchable, _ := file.LoadFile(path1, file.RegularFile, "FilesProject")

	// Create a non-searchable file (e.g. .png)
	path2 := filepath.Join(tempDir, "src/files/image.png")
	os.WriteFile(path2, []byte("image"), 0644)
	fNonSearchable, _ := file.LoadFile(path2, file.RegularFile, "FilesProject")

	p.AddFile(fSearchable)
	p.AddFile(fNonSearchable)

	if len(p.RegularFiles) != 2 {
		t.Errorf("Expected 2 RegularFiles, got %d", len(p.RegularFiles))
	}
	if len(p.SearchableRegularFiles) != 1 || p.SearchableRegularFiles[0] != fSearchable {
		t.Errorf("Expected 1 SearchableRegularFile (main.rs), got %d", len(p.SearchableRegularFiles))
	}
	if len(p.LicenseFiles) != 0 {
		t.Errorf("Expected 0 LicenseFiles, got %d", len(p.LicenseFiles))
	}
}

// TestAddFile_LicenseFile verifies that single-license files are bucketed
// correctly and effectively deduplicated if the crawler finds them twice.
func TestAddFile_LicenseFile(t *testing.T) {
	tempDir := setup(t)
	p, _ := NewProject(&readme.Readme{Licenses: make([]*readme.ReadmeLicense, 0)}, "src/licenses")

	path := filepath.Join(tempDir, "src/licenses/LICENSE")
	os.MkdirAll(filepath.Dir(path), 0755)
	os.WriteFile(path, []byte("license"), 0644)

	f, _ := file.LoadFile(path, file.SingleLicense, "LicProject")

	p.AddFile(f)
	if len(p.LicenseFiles) != 1 {
		t.Errorf("Expected 1 LicenseFile, got %d", len(p.LicenseFiles))
	}

	// Trigger deduplication loop
	p.AddFile(f)
	if len(p.LicenseFiles) != 1 {
		t.Errorf("Expected deduplication to prevent duplicate LicenseFile, got %d", len(p.LicenseFiles))
	}
}

// =========================================================================
// GetFiles and Concurrency Tests
// =========================================================================

// TestGetFiles verifies that the thread-safe getter successfully concatenates
// both regular and license files into a single slice without dropping elements.
func TestGetFiles(t *testing.T) {
	tempDir := setup(t)
	p, _ := NewProject(&readme.Readme{}, "src/concurrency")

	// We'll manually inject mock files for speed
	os.MkdirAll(filepath.Join(tempDir, "src/concurrency"), 0755)
	p.RegularFiles = []*file.File{
		func() *file.File {
			f, _ := file.LoadFile(filepath.Join(tempDir, "1.txt"), file.RegularFile, "P")
			return f
		}(),
		func() *file.File {
			f, _ := file.LoadFile(filepath.Join(tempDir, "2.txt"), file.RegularFile, "P")
			return f
		}(),
	}
	p.LicenseFiles = []*file.File{
		func() *file.File {
			f, _ := file.LoadFile(filepath.Join(tempDir, "L.txt"), file.SingleLicense, "P")
			return f
		}(),
	}

	files := p.GetFiles()
	if len(files) != 3 {
		t.Errorf("Expected GetFiles to return 3 total files, got %d", len(files))
	}
}

// TestAddFile_Concurrency validates that the p.mu Mutex successfully prevents
// slice bounds panics and race conditions when files are aggregated by multiple
// background workers simultaneously.
func TestAddFile_Concurrency(t *testing.T) {
	tempDir := setup(t)
	p, _ := NewProject(&readme.Readme{Licenses: make([]*readme.ReadmeLicense, 0)}, "src/race")
	os.MkdirAll(filepath.Join(tempDir, "src/race"), 0755)

	path := filepath.Join(tempDir, "src/race/race.txt")
	os.WriteFile(path, []byte("data"), 0644)
	f, _ := file.LoadFile(path, file.RegularFile, "RaceProject")

	var wg sync.WaitGroup
	for i := 0; i < 100; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			p.AddFile(f)
			_ = p.GetFiles()
		}()
	}
	wg.Wait()

	if len(p.RegularFiles) != 100 {
		t.Errorf("Expected 100 files to be concurrently appended safely, got %d", len(p.RegularFiles))
	}
}

// =========================================================================
// SortOrder Tests
// =========================================================================

// TestProjectOrder verifies the custom sort.Interface logic for []*Project
// to ensure projects are perfectly sorted alphabetically by their Root path.
func TestProjectOrder(t *testing.T) {
	projects := []*Project{
		{Root: "z_project"},
		{Root: "a_project"},
		{Root: "m_project"},
	}

	sort.Sort(Order(projects))

	if projects[0].Root != "a_project" || projects[1].Root != "m_project" || projects[2].Root != "z_project" {
		t.Errorf("Projects were not sorted correctly by Root: %v", projects)
	}
}
