// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package file

import (
	"bytes"
	"os"
	"path/filepath"
	"sort"
	"sync"
	"testing"

	classifierLib "github.com/google/licenseclassifier/v2"
)

// setup initializes the global state for each test.
func setup(t *testing.T) {
	Config = NewConfig()
	Config.FuchsiaDir = t.TempDir()

	allFilesMu.Lock()
	AllFiles = make(map[string]*File)
	AllLicenseFiles = make(map[string]*File)
	allFilesMu.Unlock()

	Metrics = &FileMetrics{
		counts: make(map[string]int),
		values: make(map[string][]string),
		files:  make(map[string][]byte),
	}
}

// =========================================================================
// LoadFile Tests
// =========================================================================

// TestLoadFile_Success verifies that LoadFile properly initializes a valid file.
func TestLoadFile_Success(t *testing.T) {
	setup(t)
	filename := filepath.Join(t.TempDir(), "success.txt")
	if err := os.WriteFile(filename, []byte("Example Text"), 0600); err != nil {
		t.Fatal(err)
	}

	f, err := LoadFile(filename, SingleLicense, "Example Project")
	if err != nil {
		t.Fatal(err)
	}

	if f.name != "success.txt" {
		t.Errorf("Expected name 'success.txt', got %q", f.name)
	}
	if f.projectName != "Example Project" {
		t.Errorf("Expected project name 'Example Project', got %q", f.projectName)
	}
	if f.fileType != SingleLicense {
		t.Errorf("Expected fileType SingleLicense, got %q", f.fileType)
	}
	if f.spdxID == "" {
		t.Error("Expected spdxID to be generated, got empty string")
	}
}

// TestLoadFile_EmptyFile verifies that LoadFile rejects empty files.
func TestLoadFile_EmptyFile(t *testing.T) {
	setup(t)
	filename := filepath.Join(t.TempDir(), "empty.txt")
	if err := os.WriteFile(filename, []byte(""), 0600); err != nil {
		t.Fatal(err)
	}

	_, err := LoadFile(filename, SingleLicense, "Example Project")
	if err == nil || err.Error() != "Empty file" {
		t.Fatalf("Expected 'Empty file' error, got %v", err)
	}
}

// TestLoadFile_NonExistent verifies that LoadFile correctly handles missing files.
func TestLoadFile_NonExistent(t *testing.T) {
	setup(t)
	filename := filepath.Join(t.TempDir(), "does_not_exist.txt")

	_, err := LoadFile(filename, SingleLicense, "Example Project")
	if err == nil {
		t.Fatal("Expected error for non-existent file, got nil")
	}
}

// TestLoadFile_Symlink verifies that LoadFile resolves symlinks properly.
func TestLoadFile_Symlink(t *testing.T) {
	setup(t)
	target := filepath.Join(t.TempDir(), "target.txt")
	if err := os.WriteFile(target, []byte("Example Text"), 0600); err != nil {
		t.Fatal(err)
	}
	symlink := filepath.Join(t.TempDir(), "symlink.txt")
	if err := os.Symlink(target, symlink); err != nil {
		t.Fatal(err)
	}

	f, err := LoadFile(symlink, SingleLicense, "Example Project")
	if err != nil {
		t.Fatal(err)
	}

	if f.name != "symlink.txt" {
		t.Errorf("Expected name 'symlink.txt', got %q", f.name)
	}
}

// TestLoadFile_BrokenSymlink verifies that broken symlinks are handled gracefully.
func TestLoadFile_BrokenSymlink(t *testing.T) {
	setup(t)
	symlink := filepath.Join(t.TempDir(), "broken_symlink.txt")
	// target doesn't exist
	if err := os.Symlink("/does/not/exist.txt", symlink); err != nil {
		t.Fatal(err)
	}

	_, err := LoadFile(symlink, SingleLicense, "Example Project")
	if err == nil {
		t.Fatal("Expected error for broken symlink, got nil")
	}
}

// TestLoadFile_Caching verifies that loading the same file twice returns the cached instance.
func TestLoadFile_Caching(t *testing.T) {
	setup(t)
	filename := filepath.Join(t.TempDir(), "cache.txt")
	if err := os.WriteFile(filename, []byte("Example Text"), 0600); err != nil {
		t.Fatal(err)
	}

	f1, err := LoadFile(filename, SingleLicense, "Example Project")
	if err != nil {
		t.Fatal(err)
	}

	f2, err := LoadFile(filename, SingleLicense, "Example Project")
	if err != nil {
		t.Fatal(err)
	}

	if f1 != f2 {
		t.Error("Expected LoadFile to return the exact same pointer on subsequent calls")
	}

	if Metrics.Counts()[RepeatedFileTraversal] != 1 {
		t.Errorf("Expected RepeatedFileTraversal metric to be 1, got %d", Metrics.Counts()[RepeatedFileTraversal])
	}
}

// TestLoadFile_Concurrency verifies that calling LoadFile concurrently avoids TOCTOU races.
func TestLoadFile_Concurrency(t *testing.T) {
	setup(t)
	filename := filepath.Join(t.TempDir(), "concurrency.txt")
	if err := os.WriteFile(filename, []byte("Example Text"), 0600); err != nil {
		t.Fatal(err)
	}

	var wg sync.WaitGroup
	results := make([]*File, 100)
	errors := make([]error, 100)

	for i := 0; i < 100; i++ {
		wg.Add(1)
		go func(idx int) {
			defer wg.Done()
			f, err := LoadFile(filename, SingleLicense, "Example Project")
			results[idx] = f
			errors[idx] = err
		}(i)
	}

	wg.Wait()

	var first *File
	for i, err := range errors {
		if err != nil {
			t.Fatalf("Unexpected error in goroutine %d: %v", i, err)
		}
		if i == 0 {
			first = results[i]
		} else if first != results[i] {
			t.Errorf("Expected all goroutines to receive the exact same pointer, mismatch at index %d", i)
		}
	}
}

// TestLoadFile_GlobalMaps verifies that files are correctly added to global maps based on their type.
func TestLoadFile_GlobalMaps(t *testing.T) {
	setup(t)
	regFile := filepath.Join(t.TempDir(), "regular.go")
	licFile := filepath.Join(t.TempDir(), "LICENSE")

	os.WriteFile(regFile, []byte("code"), 0600)
	os.WriteFile(licFile, []byte("license"), 0600)

	f1, _ := LoadFile(regFile, RegularFile, "Proj")
	f2, _ := LoadFile(licFile, SingleLicense, "Proj")

	allFilesMu.RLock()
	defer allFilesMu.RUnlock()

	if AllFiles[f1.AbsPath()] != f1 || AllFiles[f2.AbsPath()] != f2 {
		t.Error("Both files should be in AllFiles")
	}
	if AllLicenseFiles[f1.AbsPath()] != nil {
		t.Error("RegularFile should NOT be in AllLicenseFiles")
	}
	if AllLicenseFiles[f2.AbsPath()] != f2 {
		t.Error("SingleLicense should be in AllLicenseFiles")
	}
}

// =========================================================================
// Lazy Loading Tests
// =========================================================================

// TestLazyLoading_DeferredExecution verifies that reading the file content is deferred.
func TestLazyLoading_DeferredExecution(t *testing.T) {
	setup(t)
	filename := filepath.Join(t.TempDir(), "lazy.txt")
	os.WriteFile(filename, []byte("Content"), 0600)

	f, _ := LoadFile(filename, SingleLicense, "Proj")
	if f.contentLoaded {
		t.Error("Expected contentLoaded to be false initially")
	}
	if f.text != nil {
		t.Error("Expected text to be nil initially")
	}

	text, err := f.Text()
	if err != nil {
		t.Fatal(err)
	}

	if !f.contentLoaded {
		t.Error("Expected contentLoaded to be true after f.Text()")
	}
	if string(text) != "Content" {
		t.Errorf("Expected text 'Content', got %q", string(text))
	}
}

// TestLazyLoading_CopyrightSizeTruncation verifies that RegularFiles are truncated if CopyrightSize > 0.
func TestLazyLoading_CopyrightSizeTruncation(t *testing.T) {
	setup(t)
	Config.CopyrightSize = 10
	filename := filepath.Join(t.TempDir(), "large.go")
	os.WriteFile(filename, []byte("12345678901234567890"), 0600) // 20 bytes

	// RegularFile should be truncated
	fReg, _ := LoadFile(filename, RegularFile, "Proj")
	textReg, _ := fReg.Text()
	if len(textReg) != 10 {
		t.Errorf("Expected RegularFile to be truncated to 10 bytes, got %d", len(textReg))
	}

	// SingleLicense should NOT be truncated
	filenameLic := filepath.Join(t.TempDir(), "LICENSE")
	os.WriteFile(filenameLic, []byte("12345678901234567890"), 0600) // 20 bytes
	fLic, _ := LoadFile(filenameLic, SingleLicense, "Proj")
	textLic, _ := fLic.Text()
	if len(textLic) != 20 {
		t.Errorf("Expected SingleLicense to NOT be truncated, got %d bytes", len(textLic))
	}
}

// TestLazyLoading_CopyrightSizeZero verifies that CopyrightSize=0 doesn't truncate.
func TestLazyLoading_CopyrightSizeZero(t *testing.T) {
	setup(t)
	Config.CopyrightSize = 0
	filename := filepath.Join(t.TempDir(), "zero.go")
	os.WriteFile(filename, []byte("1234567890"), 0600) // 10 bytes

	f, _ := LoadFile(filename, RegularFile, "Proj")
	text, _ := f.Text()
	if len(text) != 10 {
		t.Errorf("Expected zero CopyrightSize to not truncate, got %d bytes", len(text))
	}
}

// TestLazyLoading_Concurrency verifies thread safety during lazy load.
func TestLazyLoading_Concurrency(t *testing.T) {
	setup(t)
	filename := filepath.Join(t.TempDir(), "lazy_concurrent.txt")
	os.WriteFile(filename, []byte("Concurrent Content"), 0600)

	f, _ := LoadFile(filename, SingleLicense, "Proj")

	var wg sync.WaitGroup
	for i := 0; i < 50; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			_, _ = f.Text()
			_, _ = f.Data()
		}()
	}
	wg.Wait()

	if !f.contentLoaded {
		t.Error("Expected content loaded to be true")
	}
}

// =========================================================================
// Memory Management Tests
// =========================================================================

// TestMemoryManagement_UnloadContent verifies that memory is released correctly.
func TestMemoryManagement_UnloadContent(t *testing.T) {
	setup(t)
	filename := filepath.Join(t.TempDir(), "unload.txt")
	os.WriteFile(filename, []byte("Unload Me"), 0600)

	f, _ := LoadFile(filename, SingleLicense, "Proj")
	_, _ = f.Text()

	if !f.contentLoaded {
		t.Fatal("Expected content to be loaded")
	}

	f.UnloadContent()

	if f.contentLoaded {
		t.Error("Expected contentLoaded to be false after UnloadContent")
	}
	if f.text != nil {
		t.Error("Expected text to be nil after UnloadContent")
	}
	if f.data != nil {
		t.Error("Expected data to be nil after UnloadContent")
	}
}

// =========================================================================
// IsPossibleLicenseFile Tests
// =========================================================================

// TestIsPossibleLicenseFile verifies that files are correctly identified or filtered.
func TestIsPossibleLicenseFile(t *testing.T) {
	tests := []struct {
		path     string
		expected bool
		desc     string
	}{
		{"LICENSE", true, "Exact match LICENSE"},
		{"COPYING", true, "Exact match COPYING"},
		{"NOTICE", true, "Exact match NOTICE"},
		{"credits.fuchsia", true, "Exact match credits.fuchsia"},
		{"my_license.txt", true, "Contains license"},
		{"tools/check-licenses/foo.go", false, "Ignored tool path"},
		{"check-license.txt", false, "Ignored tool name"},
		{"main.go", false, "Ignored extension .go"},
		{"app.JS", false, "Ignored extension case-insensitive"},
		{"readme.HTML", false, "Ignored extension HTML"},
		{"LICENSE.template", false, "Ignored substring template"},
		{"src/copying/file.c", false, "Ignored extension .c even if path contains copying"},
	}

	for _, tt := range tests {
		t.Run(tt.desc, func(t *testing.T) {
			result := IsPossibleLicenseFile(tt.path)
			if result != tt.expected {
				t.Errorf("IsPossibleLicenseFile(%q) = %v; expected %v", tt.path, result, tt.expected)
			}
		})
	}
}

// =========================================================================
// LicenseType Tests
// =========================================================================

// TestLicenseType verifies that multiple license matches are aggregated, deduplicated, and sorted.
func TestLicenseType(t *testing.T) {
	// classifierLib.Match struct contains a Name string field
	f := &File{
		contentLoaded: true,
		data: []*FileData{
			{searchResults: &classifierLib.Results{Matches: classifierLib.Matches{{Name: "MIT"}}}},
			{searchResults: &classifierLib.Results{Matches: classifierLib.Matches{{Name: "Apache-2.0"}}}},
			{searchResults: &classifierLib.Results{Matches: classifierLib.Matches{{Name: "MIT"}}}},
			{searchResults: nil}, // Handle empty results gracefully
		},
	}

	result := f.LicenseType()
	expected := "Apache-2.0, MIT"

	if result != expected {
		t.Errorf("Expected license type %q, got %q", expected, result)
	}
}

// =========================================================================
// Sub-Component Delegation Tests
// =========================================================================

// TestUpdateURLs_Delegation verifies UpdateURLs propagates to FileData elements.
func TestUpdateURLs_Delegation(t *testing.T) {
	setup(t)
	// Add config for fileDataURLs
	Config.FileDataURLs = append(Config.FileDataURLs, &FileDataURL{
		Prefix:       "https://example.com/",
		Projects:     map[string]bool{"Proj": true},
		Replacements: map[string]string{"Lib": "lib-url"},
	})

	f := &File{
		absPath:       "/prebuilt/file.txt",
		relPath:       "prebuilt/file.txt",
		contentLoaded: true,
	}
	d := &FileData{
		file:        f,
		libraryName: "Lib",
	}
	f.data = []*FileData{d}

	f.UpdateURLs("Proj", "project-url")

	if d.URL() != "https://example.com/lib-url" {
		t.Errorf("Expected URL to be updated to 'https://example.com/lib-url', got %q", d.URL())
	}
}

// =========================================================================
// SortOrder Tests
// =========================================================================

// TestSortOrder verifies that the Order slice sorts files by absolute path.
func TestSortOrder(t *testing.T) {
	files := []*File{
		{absPath: "/z/file.txt"},
		{absPath: "/a/file.txt"},
		{absPath: "/m/file.txt"},
	}

	sort.Sort(Order(files))

	if files[0].absPath != "/a/file.txt" || files[1].absPath != "/m/file.txt" || files[2].absPath != "/z/file.txt" {
		t.Errorf("Files were not sorted correctly by absPath: %v", files)
	}
}

// =========================================================================
// Replacements Tests
// =========================================================================

// TestReplacements verifies that character replacements defined in config are applied correctly.
func TestReplacements(t *testing.T) {
	setup(t)
	r := []*Replacement{
		{
			Replace: "“",
			With:    "\"",
		}, {
			Replace: "”",
			With:    "\"",
		},
	}
	Config.Replacements = r
	expected := []byte("left quote: \" right quote: \"")

	filename := filepath.Join(t.TempDir(), "replacement.txt")
	if err := os.WriteFile(filename, []byte("left quote: “ right quote: ”"), 0600); err != nil {
		t.Fatal(err)
	}

	f, err := LoadFile(filename, SingleLicense, "Example Project")
	if err != nil {
		t.Fatal(err)
	}
	data, err := f.Data()
	if err != nil {
		t.Fatal(err)
	}
	if len(data) != 1 {
		t.Fatalf("Expected 1 data element, got %v\n", len(data))
	}
	if !bytes.Equal(data[0].Data(), expected) {
		t.Fatalf("Expected %v, got %v\n", string(expected), string(data[0].Data()))
	}
}
