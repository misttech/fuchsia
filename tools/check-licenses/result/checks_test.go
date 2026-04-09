// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package result

import (
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"

	classifierLib "github.com/google/licenseclassifier/v2"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/directory"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/file"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/project"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/readme"
)

// resetState clears the global state across packages to ensure isolated tests.
func resetState(t *testing.T) string {
	t.Helper()
	tempDir := t.TempDir()

	file.Config = file.NewConfig()
	file.Config.FuchsiaDir = tempDir

	project.Config = project.NewConfig()
	project.Config.FuchsiaDir = tempDir
	project.InitializeForTest()

	directory.Config = directory.NewConfig()
	directory.Config.FuchsiaDir = tempDir
	directory.InitializeForTest()

	readme.InitializeForTest()

	Config = NewConfig()
	Config.FuchsiaDir = tempDir

	return tempDir
}

// =========================================================================
// Validation Linter Tests (checks.go)
// =========================================================================

func TestCheck_FuchsiaCopyrightHeaders(t *testing.T) {
	tempDir := resetState(t)

	// Create Fuchsia project
	fuchsia := &project.Project{Root: tempDir, Name: "fuchsia", ReadmeFile: &readme.Readme{}}
	project.AddFilteredProject(fuchsia)
	Config.FuchsiaDir = tempDir

	file.Config.Extensions[".cc"] = true

	// Create mock file
	filePath := filepath.Join(tempDir, "main.cc")
	os.WriteFile(filePath, []byte("code"), 0644)
	f, _ := file.LoadFile(filePath, file.RegularFile, "fuchsia")
	f.SetURL("http://example.com")

	fuchsia.AddFile(f)

	// FAIL Case: File exists but doesn't have the FuchsiaCopyright match
	err := AllFuchsiaAuthorSourceFilesMustHaveCopyrightHeaders()
	if err == nil {
		t.Fatal("Expected copyright check to fail on unparsed/unmatched file")
	}

	// PASS Case: Add to allowlist
	Config.Checks = []*Check{
		{
			Name: "AllFuchsiaAuthorSourceFilesMustHaveCopyrightHeaders",
			Allowlist: map[string]bool{
				f.RelPath(): true,
			},
		},
	}
	err = AllFuchsiaAuthorSourceFilesMustHaveCopyrightHeaders()
	if err != nil {
		t.Fatalf("Expected copyright check to pass with allowlist, got: %v", err)
	}
}

func TestCheck_UnrecognizedLicenseTexts(t *testing.T) {
	tempDir := resetState(t)

	p := &project.Project{Root: "src/foo", Name: "Foo", ReadmeFile: &readme.Readme{}}
	project.AddFilteredProject(p)

	filePath := filepath.Join(tempDir, "LICENSE")
	os.WriteFile(filePath, []byte("Unrecognized Text"), 0644)
	f, _ := file.LoadFile(filePath, file.SingleLicense, "Foo")

	// Hack to simulate a nil SearchResults
	f.SetURL("http://example.com")
	p.AddFile(f)

	// FAIL Case
	err := AllLicenseTextsMustBeRecognized()
	if err == nil || !strings.Contains(err.Error(), "Found unrecognized license texts") {
		t.Fatalf("Expected unrecognized license text error, got: %v", err)
	}
}

func TestCheck_UnapprovedLicensePatterns(t *testing.T) {
	tempDir := resetState(t)

	p := &project.Project{Root: "src/foo", Name: "Foo", ReadmeFile: &readme.Readme{}}
	project.AddFilteredProject(p)

	filePath := filepath.Join(tempDir, "LICENSE")
	os.WriteFile(filePath, []byte("Restricted License"), 0644)
	f, _ := file.LoadFile(filePath, file.SingleLicense, "Foo")
	f.SetURL("http://example.com")
	p.AddFile(f)

	// Inject a fake search result
	data, _ := f.Data()
	data[0].SetData([]byte("Restricted License"))

	// Override with a restricted match
	data[0].SetSearchResultsForTest(&classifierLib.Results{
		Matches: classifierLib.Matches{{Name: "GPL-3.0", MatchType: "Restricted"}},
	})

	// FAIL Case
	err := AllLicensePatternUsagesMustBeApproved()
	if err == nil || !strings.Contains(err.Error(), "not approved to use license pattern GPL-3.0") {
		t.Fatalf("Expected unapproved license pattern error, got: %v", err)
	}

	// PASS Case: Add to allowlist
	Config.AllowLists = []*AllowList{
		{
			Name:      "GPL-3.0",
			MatchType: "Restricted",
			Entries: []*AllowListEntry{
				{Projects: []string{"src/foo"}},
			},
		},
	}
	err = AllLicensePatternUsagesMustBeApproved()
	if err != nil {
		t.Fatalf("Expected check to pass with global allowlist, got: %v", err)
	}
}

func TestCheck_AllProjectsHaveLicenses(t *testing.T) {
	resetState(t)

	p := &project.Project{Root: "src/nolicense", Name: "NoLicense", ReadmeFile: &readme.Readme{}}
	project.AddFilteredProject(p)

	// FAIL Case
	err := AllProjectsMustHaveALicense()
	if err == nil || !strings.Contains(err.Error(), "without any license information") {
		t.Fatalf("Expected missing license error, got: %v", err)
	}

	// PASS Case
	Config.Checks = []*Check{
		{
			Name: "AllProjectsMustHaveALicense",
			Allowlist: map[string]bool{
				"src/nolicense": true,
			},
		},
	}
	err = AllProjectsMustHaveALicense()
	if err != nil {
		t.Fatalf("Expected check to pass with allowlist, got: %v", err)
	}
}

func TestCheck_ComplianceLinksAreGood(t *testing.T) {
	tempDir := resetState(t)
	Config.CheckURLs = true

	// Spin up mock HTTP server
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/good" {
			w.WriteHeader(http.StatusOK)
		} else {
			w.WriteHeader(http.StatusNotFound)
		}
	}))
	defer server.Close()

	p := &project.Project{Root: "src/links", Name: "Links", ReadmeFile: &readme.Readme{}}
	project.AddFilteredProject(p)

	pathGood := filepath.Join(tempDir, "LICENSE.good")
	os.WriteFile(pathGood, []byte("good"), 0644)
	fGood, _ := file.LoadFile(pathGood, file.SingleLicense, "Links")
	fGood.SetURL(server.URL + "/good")
	p.AddFile(fGood)

	pathBad := filepath.Join(tempDir, "LICENSE.bad")
	os.WriteFile(pathBad, []byte("bad"), 0644)
	fBad, _ := file.LoadFile(pathBad, file.SingleLicense, "Links")
	fBad.SetURL(server.URL + "/bad")
	p.AddFile(fBad)

	// Force initialization of data
	fGood.Data()
	fBad.Data()

	err := AllComplianceWorksheetLinksAreGood()
	if err == nil || !strings.Contains(err.Error(), "Encountered 1 bad license URLs") {
		t.Fatalf("Expected crawler to catch the 404 bad link, got: %v", err)
	}
}
