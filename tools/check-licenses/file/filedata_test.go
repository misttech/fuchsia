// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package file

import (
	"bytes"
	"sort"
	"sync"
	"testing"
)

// =========================================================================
// LoadFileData Tests (File Type Parsing)
// =========================================================================

// TestLoadFileData_UnknownType verifies that attempting to load a file with an
// unregistered or unknown FileType correctly returns an error.
func TestLoadFileData_UnknownType(t *testing.T) {
	setup(t)
	f := &File{fileType: FileType("UnknownType")}
	_, err := LoadFileData(f, []byte("text"))
	if err == nil {
		t.Errorf("Expected error for unknown file type")
	}
}

// TestLoadFileData_SingleLicense validates the core LoadFileData parsing path for a
// SingleLicense (and RegularFile). It verifies that exactly one FileData object is
// returned with trimmed text, and asserts that the LibraryName, URL, SPDXName,
// and dynamically generated SPDXID are all instantiated properly.
func TestLoadFileData_SingleLicense(t *testing.T) {
	setup(t)
	f := &File{
		fileType:    SingleLicense,
		projectName: "TestProject",
		url:         "http://example.com",
		relPath:     "path/to/file",
	}
	data, err := LoadFileData(f, []byte("  license text  \n"))
	if err != nil {
		t.Fatal(err)
	}

	if len(data) != 1 {
		t.Fatalf("Expected 1 FileData, got %d", len(data))
	}

	fd := data[0]
	if string(fd.Data()) != "license text" {
		t.Errorf("Expected trimmed 'license text', got %q", string(fd.Data()))
	}
	if fd.LibraryName() != "TestProject" {
		t.Errorf("Expected LibraryName to be 'TestProject', got %q", fd.LibraryName())
	}
	if fd.URL() != "http://example.com" {
		t.Errorf("Expected URL to be 'http://example.com', got %q", fd.URL())
	}
	if fd.SPDXName() != "TestProject" {
		t.Errorf("Expected SPDXName to be 'TestProject', got %q", fd.SPDXName())
	}
	if fd.SPDXID() == "" {
		t.Errorf("Expected SPDXID to be generated")
	}
}

// TestLoadFileData_MultiLicense validates that MultiLicense types correctly
// delegate parsing to the specific parsers (e.g., ParseChromium) and return
// multiple FileData segments with correct line numbers and library names.
func TestLoadFileData_MultiLicense(t *testing.T) {
	setup(t)
	f := &File{
		fileType:    MultiLicenseChromium,
		projectName: "ChromiumProject",
		relPath:     "path/to/NOTICE",
	}

	// Mocking a Chromium NOTICE format
	content := []byte("\n--------------------\nlibraryA\n--------------------\nlicense A text\n--------------------\nlibraryB\n--------------------\nlicense B text\n")
	data, err := LoadFileData(f, content)
	if err != nil {
		t.Fatal(err)
	}

	if len(data) != 2 {
		t.Fatalf("Expected 2 FileData segments, got %d", len(data))
	}

	if data[0].LibraryName() != "libraryA" || string(data[0].Data()) != "license A text" {
		t.Errorf("First segment parsed incorrectly: %v, %s", data[0].LibraryName(), string(data[0].Data()))
	}
	if data[1].LibraryName() != "libraryB" || string(data[1].Data()) != "license B text" {
		t.Errorf("Second segment parsed incorrectly: %v, %s", data[1].LibraryName(), string(data[1].Data()))
	}
}

// =========================================================================
// Replacements Engine Tests
// =========================================================================

// TestReplacementsEngine verifies that characters specified in Config.Replacements
// are correctly substituted during LoadFileData execution.
func TestReplacementsEngine(t *testing.T) {
	setup(t)
	Config.Replacements = []*Replacement{
		{Replace: "badchar", With: "goodchar"},
	}

	f := &File{fileType: SingleLicense, projectName: "Proj", relPath: "path"}
	data, err := LoadFileData(f, []byte("text with badchar here"))
	if err != nil {
		t.Fatal(err)
	}

	if string(data[0].Data()) != "text with goodchar here" {
		t.Errorf("Replacement failed, got %q", string(data[0].Data()))
	}
}

// =========================================================================
// SPDX ID Generation & Updating Tests
// =========================================================================

// TestSPDXIDGeneration verifies that SPDX IDs are generated on load,
// updated dynamically when SetData is called, and guaranteed to be unique.
func TestSPDXIDGeneration(t *testing.T) {
	setup(t)
	f1 := &File{fileType: SingleLicense, projectName: "Proj1", relPath: "path1"}
	f2 := &File{fileType: SingleLicense, projectName: "Proj2", relPath: "path2"}

	data1, _ := LoadFileData(f1, []byte("same text"))
	data2, _ := LoadFileData(f2, []byte("same text"))

	id1 := data1[0].SPDXID()
	id2 := data2[0].SPDXID()

	// No Hash Collisions
	if id1 == id2 {
		t.Errorf("Expected different SPDX IDs for different projects/paths, got %s", id1)
	}

	// Update via SetData
	data1[0].SetData([]byte("different text"))
	newID1 := data1[0].SPDXID()

	if newID1 == id1 {
		t.Errorf("Expected SPDX ID to be re-generated and different after SetData")
	}
}

// =========================================================================
// UpdateURLs Behavior Tests
// =========================================================================

// TestUpdateURLs extensively tests the prebuilt URL resolution logic across three scenarios:
// 1. A non-prebuilt file (ensures the URL remains untouched).
// 2. A matching prebuilt file (ensures the replacement config is applied and URL is rewritten).
// 3. A mismatched prebuilt file (ensures if the project name is wrong, it skips rewriting).
func TestUpdateURLs(t *testing.T) {
	setup(t)
	Config.FileDataURLs = []*FileDataURL{
		{
			Prefix: "http://prefix.com/",
			Projects: map[string]bool{
				"TargetProject": true,
			},
			Replacements: map[string]string{
				"TargetLibrary": "suffix/path",
			},
		},
	}

	// Standard Project (Not a prebuilt)
	fdNormal := &FileData{
		file:        &File{relPath: "src/file"},
		libraryName: "TargetLibrary",
		url:         "http://original.com",
	}
	fdNormal.UpdateURLs("TargetProject", "http://project.com")
	if fdNormal.URL() != "http://original.com" {
		t.Errorf("Expected URL to remain unchanged for non-prebuilt, got %q", fdNormal.URL())
	}

	// Prebuilt Override, matching project and library
	fdPrebuilt := &FileData{
		file:        &File{relPath: "prebuilt/file"},
		libraryName: "TargetLibrary",
		url:         "http://original.com",
	}
	fdPrebuilt.UpdateURLs("TargetProject", "http://project.com")
	if fdPrebuilt.URL() != "http://prefix.com/suffix/path" {
		t.Errorf("Expected URL to be updated, got %q", fdPrebuilt.URL())
	}

	// Prebuilt, wrong project
	fdPrebuiltWrongProject := &FileData{
		file:        &File{relPath: "prebuilt/file"},
		libraryName: "TargetLibrary",
		url:         "http://original.com",
	}
	fdPrebuiltWrongProject.UpdateURLs("WrongProject", "http://project.com")
	if fdPrebuiltWrongProject.URL() != "http://original.com" {
		t.Errorf("Expected URL to remain unchanged for wrong project, got %q", fdPrebuiltWrongProject.URL())
	}
}

// =========================================================================
// Deduplication Hashing Tests
// =========================================================================

// TestHashAndSetData tests the deterministic output of Hash() and verifies that calling SetData
// correctly clears the cached hash string, ensuring subsequent calls generate the new hash.
func TestHashAndSetData(t *testing.T) {
	fd := &FileData{
		data: []byte("  initial data  "),
		file: &File{relPath: "path"},
	}

	h1 := fd.Hash()
	if h1 == "" {
		t.Errorf("Expected a valid hash")
	}

	// Hash should be cached deterministically
	h2 := fd.Hash()
	if h1 != h2 {
		t.Errorf("Expected hash to be cached and identical")
	}

	// SetData should clear cache
	fd.SetData([]byte("  new data  "))
	h3 := fd.Hash()
	if h3 == "" || h3 == h1 {
		t.Errorf("Expected a new valid hash after SetData, got %q", h3)
	}

	if string(fd.Data()) != "  new data  " {
		t.Errorf("Expected data to be updated")
	}
}

// =========================================================================
// Concurrency & Data Race Safety Tests
// =========================================================================

// TestFileData_Concurrency verifies that calling getters and setters simultaneously
// on a FileData object does not result in data races or panics.
func TestFileData_Concurrency(t *testing.T) {
	fd := &FileData{
		data:        []byte("concurrent data"),
		libraryName: "concurrent lib",
		url:         "http://initial.com",
		file:        &File{relPath: "prebuilt/file"},
	}

	setup(t)
	Config.FileDataURLs = []*FileDataURL{{
		Prefix:       "http://prefix.com/",
		Projects:     map[string]bool{"Proj": true},
		Replacements: map[string]string{"concurrent lib": "url"},
	}}

	var wg sync.WaitGroup

	// Spin up readers
	for i := 0; i < 50; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			_ = fd.Data()
			_ = fd.URL()
			_ = fd.SPDXID()
			_ = fd.Hash()
		}()
	}

	// Spin up writers
	for i := 0; i < 10; i++ {
		wg.Add(1)
		go func(idx int) {
			defer wg.Done()
			fd.SetData(bytes.Repeat([]byte("a"), idx))
			fd.UpdateURLs("Proj", "")
		}(i)
	}

	wg.Wait()
}

// TestSearch_DoubleCheckedLock verifies that calling Search() concurrently
// safely initializes the classifier results exactly once without panicking.
func TestSearch_DoubleCheckedLock(t *testing.T) {
	// Initialize classifier so it doesn't nil panic
	Initialize(&FileConfig{ClassifierThreshold: 0.8})

	fd := &FileData{
		data: []byte("License text to classify"),
	}

	var wg sync.WaitGroup
	for i := 0; i < 50; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			fd.Search()
		}()
	}

	wg.Wait()

	if fd.SearchResults() == nil {
		t.Errorf("Expected search results to be initialized")
	}
}

// =========================================================================
// SortOrder Tests
// =========================================================================

// TestOrderFileData verifies the custom sorting logic for OrderFileData slice type.
// It ensures that FileData objects are correctly ordered alphabetically by their
// absolute file path first, and then numerically by their line number within the file.
func TestOrderFileData(t *testing.T) {
	fd1 := &FileData{file: &File{absPath: "b/path"}, lineNumber: 10}
	fd2 := &FileData{file: &File{absPath: "a/path"}, lineNumber: 5}
	fd3 := &FileData{file: &File{absPath: "a/path"}, lineNumber: 2}

	list := OrderFileData{fd1, fd2, fd3}
	sort.Sort(list)

	if list[0] != fd3 || list[1] != fd2 || list[2] != fd1 {
		t.Errorf("OrderFileData failed to sort correctly")
	}
}
