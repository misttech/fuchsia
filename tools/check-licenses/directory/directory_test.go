// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package directory

import (
	"os"
	"path/filepath"
	"testing"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/file"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/project"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/readme"
)

// setup initializes the global state across packages to ensure isolation between tests.
func setup(t *testing.T) string {
	t.Helper()
	tempDir := t.TempDir()

	file.Config = file.NewConfig()
	file.Config.FuchsiaDir = tempDir

	Config = NewConfig()
	Config.FuchsiaDir = tempDir

	allDirectoriesMu.Lock()
	allDirectories = make(map[string]*Directory)
	allDirectoriesMu.Unlock()
	RootDirectory = nil

	project.Config = project.NewConfig()
	project.Config.FuchsiaDir = tempDir
	project.Config.Barriers = []*project.Barrier{
		{Paths: []string{"third_party"}},
	}
	project.InitializeForTest()
	project.UnknownProject = &project.Project{
		Name:       "unknown",
		ReadmeFile: &readme.Readme{},
	}

	readme.InitializeForTest()

	return tempDir
}

// =========================================================================
// Project Inheritance and Barriers Tests
// =========================================================================
// TestProjectInheritance verifies that subdirectories inherit the Project of their parent
// unless they are explicitly blocked by a barrier.
func TestProjectInheritance(t *testing.T) {
	tempDir := setup(t)
	root := filepath.Join(tempDir, "root")
	child := filepath.Join(root, "child")
	os.MkdirAll(child, 0755)

	mockProject := &project.Project{Name: "MockProject", ReadmeFile: &readme.Readme{}}
	parentDir := &Directory{Path: root, Project: mockProject}

	d, err := newDirectoryWithConfig(child, parentDir, Config)
	if err != nil {
		t.Fatal(err)
	}

	if d.Project != mockProject {
		t.Errorf("Expected child directory to inherit parent's project, got %v", d.Project)
	}
}

// TestProjectBarrier verifies that "third_party" or "prebuilt" directories correctly
// reset the inherited Project to UnknownProject, forcing them to establish their own.
func TestProjectBarrier(t *testing.T) {
	tempDir := setup(t)
	barrierDir := filepath.Join(tempDir, "third_party")
	os.MkdirAll(barrierDir, 0755)

	mockProject := &project.Project{Name: "MockProject", ReadmeFile: &readme.Readme{}}
	parentDir := &Directory{Path: tempDir, Project: mockProject}

	d, err := newDirectoryWithConfig(barrierDir, parentDir, Config)
	if err != nil {
		t.Fatal(err)
	}

	if d.Project == mockProject {
		t.Error("Expected third_party directory to block parent project inheritance")
	}
	if d.Project != project.UnknownProject {
		t.Errorf("Expected barrier directory to reset to UnknownProject, got %v", d.Project)
	}
}

// =========================================================================
// README Processing and Project Creation Tests
// =========================================================================

// TestReadme_ValidFuchsia verifies that finding a README.fuchsia correctly parses it
// and creates a new project associated with this directory.
func TestReadme_ValidFuchsia(t *testing.T) {
	tempDir := setup(t)
	root := filepath.Join(tempDir, "with_readme")
	os.MkdirAll(root, 0755)

	readmeContent := `Name: TestProject
URL: http://test
Version: 1.0
Revision: abc
Security Critical: no
License: MIT
License File: LICENSE
`
	os.WriteFile(filepath.Join(root, "README.fuchsia"), []byte(readmeContent), 0644)
	os.WriteFile(filepath.Join(root, "LICENSE"), []byte("license"), 0644)

	d, err := newDirectoryWithConfig(root, nil, Config)
	if err != nil {
		t.Fatal(err)
	}

	if d.Project == nil || d.Project == project.UnknownProject {
		t.Fatal("Expected a new project to be created from README.fuchsia")
	}
	if d.Project.Name != "TestProject" {
		t.Errorf("Expected Project Name 'TestProject', got %q", d.Project.Name)
	}
}

// TestReadme_Malformed verifies that severely broken READMEs correctly abort traversal.
func TestReadme_Malformed(t *testing.T) {
	tempDir := setup(t)
	root := filepath.Join(tempDir, "bad_readme")
	os.MkdirAll(root, 0755)

	// A README that triggers a parser error (e.g. unknown deprecated directive)
	os.WriteFile(filepath.Join(root, "README.fuchsia"), []byte("check-licenses: unknown_directive"), 0644)

	_, err := newDirectoryWithConfig(root, nil, Config)
	if err == nil {
		t.Fatal("Expected error when parsing severely malformed README")
	}
}

// TestReadme_PreExistingProject verifies that if a project is already registered in the
// AllProjects map for a given directory, the traversal engine cleanly reuses it.
func TestReadme_PreExistingProject(t *testing.T) {
	tempDir := setup(t)
	root := filepath.Join(tempDir, "cached_project")
	os.MkdirAll(root, 0755)

	mockProject := &project.Project{Name: "PreCachedProject", ReadmeFile: &readme.Readme{}}

	// project.AllProjects is keyed by paths relative to FuchsiaDir
	relRoot, _ := filepath.Rel(tempDir, root)
	mockProject.Root = relRoot
	project.AddProject(mockProject)
	d, err := newDirectoryWithConfig(root, nil, Config)
	if err != nil {
		t.Fatal(err)
	}

	if d.Project != mockProject {
		t.Errorf("Expected directory to re-use cached project, got %v", d.Project.Name)
	}
}

// =========================================================================
// Traversal Engine Tests
// =========================================================================

// TestTraversal_FileClassification verifies that license files and standard source files
// are properly bucketed into their correct enums before being passed to the file package.
func TestTraversal_FileClassification(t *testing.T) {
	tempDir := setup(t)
	root := filepath.Join(tempDir, "files")
	os.MkdirAll(root, 0755)

	os.WriteFile(filepath.Join(root, "LICENSE"), []byte("license"), 0644)
	os.WriteFile(filepath.Join(root, "main.cc"), []byte("code"), 0644)

	d, err := newDirectoryWithConfig(root, nil, Config)
	if err != nil {
		t.Fatal(err)
	}

	if len(d.Files) != 2 {
		t.Fatalf("Expected 2 files traversed, got %d", len(d.Files))
	}

	for _, f := range d.Files {
		if f.Name() == "LICENSE" && f.FileType() != file.SingleLicense {
			t.Errorf("Expected LICENSE to be classified as SingleLicense, got %v", f.FileType())
		}
		if f.Name() == "main.cc" && f.FileType() != file.RegularFile {
			t.Errorf("Expected main.cc to be classified as RegularFile, got %v", f.FileType())
		}
	}
}

// TestTraversal_Skips verifies that the DirectoryConfig Skips list prevents specified
// files or directories from being processed entirely.
func TestTraversal_Skips(t *testing.T) {
	tempDir := setup(t)
	root := filepath.Join(tempDir, "skips")
	os.MkdirAll(filepath.Join(root, "ignore_me_folder"), 0755)
	os.WriteFile(filepath.Join(root, "keep.txt"), []byte("keep"), 0644)
	os.WriteFile(filepath.Join(root, "ignore.txt"), []byte("ignore"), 0644)

	Config.Skips = []*Skip{
		{Paths: []string{filepath.Join(root, "ignore_me_folder"), filepath.Join(root, "ignore.txt")}},
	}

	d, err := newDirectoryWithConfig(root, nil, Config)
	if err != nil {
		t.Fatal(err)
	}

	if len(d.Children) != 0 {
		t.Errorf("Expected ignore_me_folder to be skipped, but found %d children", len(d.Children))
	}
	if len(d.Files) != 1 || d.Files[0].Name() != "keep.txt" {
		t.Errorf("Expected only keep.txt to be traversed, but got %v", d.Files)
	}
}

// TestTraversal_Sorting verifies that Child directories are always alphabetically sorted.
func TestTraversal_Sorting(t *testing.T) {
	tempDir := setup(t)
	root := filepath.Join(tempDir, "sort")
	os.MkdirAll(filepath.Join(root, "Z_folder"), 0755)
	os.MkdirAll(filepath.Join(root, "A_folder"), 0755)
	os.MkdirAll(filepath.Join(root, "M_folder"), 0755)

	d, err := newDirectoryWithConfig(root, nil, Config)
	if err != nil {
		t.Fatal(err)
	}

	if len(d.Children) != 3 {
		t.Fatalf("Expected 3 children, got %d", len(d.Children))
	}

	if d.Children[0].Name != "A_folder" || d.Children[1].Name != "M_folder" || d.Children[2].Name != "Z_folder" {
		t.Errorf("Expected children to be alphabetically sorted, got: %s, %s, %s",
			d.Children[0].Name, d.Children[1].Name, d.Children[2].Name)
	}
}

// =========================================================================
// Symlink Edge Cases Tests
// =========================================================================

// TestSymlink_Directory verifies that symlinks pointing to directories are aggressively
// skipped to prevent treating them as files and to prevent infinite recursive loops.
func TestSymlink_Directory(t *testing.T) {
	tempDir := setup(t)
	root := filepath.Join(tempDir, "symlink_dir")
	target := filepath.Join(tempDir, "target_dir")
	os.MkdirAll(root, 0755)
	os.MkdirAll(target, 0755)

	symlinkPath := filepath.Join(root, "link_to_target")
	if err := os.Symlink(target, symlinkPath); err != nil {
		t.Fatal(err)
	}

	d, err := newDirectoryWithConfig(root, nil, Config)
	if err != nil {
		t.Fatal(err)
	}

	if len(d.Children) != 0 {
		t.Errorf("Expected directory symlinks to be skipped, got %d children", len(d.Children))
	}
	if len(d.Files) != 0 {
		t.Errorf("Expected directory symlinks NOT to be tracked as files, got %d files", len(d.Files))
	}
}

// TestSymlink_File verifies that valid file symlinks are properly passed to the file package.
func TestSymlink_File(t *testing.T) {
	tempDir := setup(t)
	root := filepath.Join(tempDir, "symlink_file")
	os.MkdirAll(root, 0755)

	targetFile := filepath.Join(tempDir, "target.txt")
	os.WriteFile(targetFile, []byte("target"), 0644)

	symlinkPath := filepath.Join(root, "link.txt")
	if err := os.Symlink(targetFile, symlinkPath); err != nil {
		t.Fatal(err)
	}

	d, err := newDirectoryWithConfig(root, nil, Config)
	if err != nil {
		t.Fatal(err)
	}

	if len(d.Files) != 1 {
		t.Fatalf("Expected file symlink to be loaded, got %d files", len(d.Files))
	}
	if d.Files[0].Name() != "link.txt" {
		t.Errorf("Expected file name 'link.txt', got %q", d.Files[0].Name())
	}
}

// TestSymlink_Broken verifies that broken symlinks are swallowed and don't panic the traversal.
func TestSymlink_Broken(t *testing.T) {
	tempDir := setup(t)
	root := filepath.Join(tempDir, "symlink_broken")
	os.MkdirAll(root, 0755)

	symlinkPath := filepath.Join(root, "broken.txt")
	if err := os.Symlink("/does/not/exist.txt", symlinkPath); err != nil {
		t.Fatal(err)
	}

	d, err := newDirectoryWithConfig(root, nil, Config)
	if err != nil {
		t.Fatalf("Expected broken symlink error to be swallowed, instead got: %v", err)
	}

	if len(d.Files) != 0 {
		t.Errorf("Expected broken symlink to be ignored, got %d files", len(d.Files))
	}
}

// =========================================================================
// Global State Registration Tests
// =========================================================================

// TestGlobalRegistration verifies that instantiated directories are safely
// placed into the global allDirectories map.
func TestGlobalRegistration(t *testing.T) {
	tempDir := setup(t)
	root := filepath.Join(tempDir, "global")
	child := filepath.Join(root, "child")
	os.MkdirAll(child, 0755)

	d, err := newDirectoryWithConfig(root, nil, Config)
	if err != nil {
		t.Fatal(err)
	}

	if RootDirectory != d {
		t.Error("Expected RootDirectory to point to the highest level Directory")
	}

	all := GetAllDirectories()
	if len(all) != 2 {
		t.Fatalf("Expected 2 total directories registered, got %d", len(all))
	}

	if _, ok := all[root]; !ok {
		t.Error("Root directory was not added to global map")
	}
	if _, ok := all[child]; !ok {
		t.Error("Child directory was not added to global map")
	}
}
