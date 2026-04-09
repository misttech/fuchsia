// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package result

import (
	"embed"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"
	spdx_common "github.com/spdx/tools-golang/spdx/common"
	spdx "github.com/spdx/tools-golang/spdx/v2_2"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/file"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/project"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/testutil"
)

//go:embed testdata/*
var testDataFS embed.FS

// If the root project is null, SPDX doc generation should fail.
func TestDocCreationEmpty(t *testing.T) {
	dir := t.TempDir()
	Config = &ResultConfig{
		FuchsiaDir: dir,
		OutDir:     dir,
	}
	_, err := generateSPDXDoc(t.Name(), []*project.Project{}, nil)
	if err == nil {
		t.Fatalf("%s: expected error, got none.", t.Name())
	}
}

// Simple case: Ensure we can generate a simple doc with one SPDX package.
func TestDocCreationOnePackage(t *testing.T) { runTest("one_package", t) }

// Ensure we can create an SPDX document with multiple packages.
// The root package must have a "CONTAINS" relationship on all other packages.
func TestDocCreationMultiPackage(t *testing.T) { runTest("multi_package", t) }

// Ensure license information is presented properly in the SPDX document.
func TestDocCreationMultiPackageOneLicense(t *testing.T) { runTest("multi_package_one_license", t) }

// If a given package has multiple license files, ensure they are all included in
// the SPDX document successfully.
func TestDocCreationMultiPackageMultiLicense(t *testing.T) { runTest("multi_package_multi_license", t) }

// Similar to the above, NOTICE files have multiple license texts. Ensure we
// handle that situation properly.
func TestDocCreationMultiPackageNotice(t *testing.T) { runTest("multi_package_one_notice", t) }

func runTest(folder string, t *testing.T) {
	tempDir := t.TempDir()
	testutil.DumpTestData(t, testDataFS, tempDir)
	testDataDir := filepath.Join(tempDir, "testdata")
	projects := loadProjects(tempDir, folder)
	root := projects[0]

	_, err := generateSPDXDoc(t.Name(), projects, root)
	if err != nil {
		t.Fatalf("%s: expected no error, got %v", t.Name(), err)
	}

	wantPath := filepath.Join(testDataDir, "spdx", folder, "want.json")
	gotPath := filepath.Join(Config.OutDir, spdxFilename)
	want, got := loadWantGot(wantPath, gotPath, t)

	if d := cmp.Diff(want, got); d != "" {
		t.Errorf("%v: compare docs mismatch: (-want +got):\n%s", t.Name(), d)
	}
}

func addLicense(tempDir string, p *project.Project, relPath string, fileType file.FileType, content string) {
	absPath := filepath.Join(tempDir, relPath)
	os.MkdirAll(filepath.Dir(absPath), 0755)
	os.WriteFile(absPath, []byte(content), 0644)
	f, _ := file.LoadFile(absPath, fileType, p.Name)
	f.SetURL("www.example.com")
	p.LicenseFiles = append(p.LicenseFiles, f)
}

func loadProjects(tempDir, folder string) []*project.Project {
	file.Config.FuchsiaDir = tempDir
	switch folder {
	case "one_package":
		return []*project.Project{
			{Root: "fake/path", Name: "One Package Test Project", SPDXID: "Package-3499410769"},
		}
	case "multi_package":
		p1 := &project.Project{Root: "fake/path", Name: "PackageA", SPDXID: "Package-72237883"}
		addLicense(tempDir, p1, "fake/multi_package/path", file.SingleLicense, "Example License Text for PackageA")
		return []*project.Project{
			p1,
			{Root: "fake/path2", Name: "PackageB", SPDXID: "Package-852005636"},
			{Root: "fake/path3", Name: "PackageC", SPDXID: "Package-1864736318"},
		}
	case "multi_package_one_license":
		p1 := &project.Project{Root: "fake/path", Name: "PackageA", SPDXID: "Package-72237883"}
		addLicense(tempDir, p1, "fake/multi_package_one_license/path", file.SingleLicense, "Testing Testing 123")
		return []*project.Project{
			p1,
			{Root: "fake/path2", Name: "PackageB", SPDXID: "Package-852005636"},
			{Root: "fake/path3", Name: "PackageC", SPDXID: "Package-1864736318"},
		}
	case "multi_package_multi_license":
		p1 := &project.Project{Root: "fake/path", Name: "PackageA", SPDXID: "Package-72237883"}
		addLicense(tempDir, p1, "fake/multi_package_multi_license/path1", file.SingleLicense, "package A 1st license file")
		addLicense(tempDir, p1, "fake/multi_package_multi_license/path2", file.SingleLicense, "package A 2nd license file")
		addLicense(tempDir, p1, "fake/multi_package_multi_license/path3", file.SingleLicense, "package A 3rd license file")
		p2 := &project.Project{Root: "fake/path2", Name: "PackageB", SPDXID: "Package-852005636"}
		addLicense(tempDir, p2, "fake/multi_package_multi_license_b/path", file.SingleLicense, "package B license file")
		return []*project.Project{
			p1,
			p2,
			{Root: "fake/path3", Name: "PackageC", SPDXID: "Package-1864736318"},
		}
	case "multi_package_one_notice":
		p1 := &project.Project{Root: "fake/path", Name: "ProjectA", SPDXID: "Package-257155980"}
		addLicense(tempDir, p1, "fake/multi_package/path", file.MultiLicenseGoogle, "ProjectA\nTesting 123 1\n=================\nProjectA\nTesting 123 2\n=================\nProjectA\nTesting 123 3\n=================")
		p2 := &project.Project{Root: "fake/path2", Name: "ProjectB", SPDXID: "Package-3300990661"}
		addLicense(tempDir, p2, "fake/multi_package/path2", file.MultiLicenseGoogle, "ProjectB\nTesting 123 4\n=================")
		return []*project.Project{
			p1,
			p2,
			{Root: "fake/path3", Name: "PackageC", SPDXID: "Package-1864736318"},
		}
	}
	return nil
}

func loadWantGot(wantPath, gotPath string, t *testing.T) (*spdx.Document, *spdx.Document) {
	t.Helper()

	content, err := os.ReadFile(wantPath)
	if err != nil {
		t.Fatalf("%s: failed to read in json file [%s]: %v", t.Name(), wantPath, err)
	}

	var want *spdx.Document
	err = json.Unmarshal(content, &want)
	if err != nil {
		t.Fatalf("%s: failed to unmarshal data [%s]: %v", t.Name(), wantPath, err)
	}

	content, err = os.ReadFile(gotPath)
	if err != nil {
		t.Fatalf("%s: failed to read in json file [%s]: %v", t.Name(), gotPath, err)
	}

	// Update golden files
	// os.WriteFile(filepath.Join("../../tools/check-licenses/result/testdata/spdx", filepath.Base(filepath.Dir(wantPath)), "want.json"), content, 0644)

	var got *spdx.Document
	err = json.Unmarshal(content, &got)
	if err != nil {
		t.Fatalf("%s: failed to unmarshal data [%s]: %v", t.Name(), gotPath, err)
	}

	// Unable to accurately set the creation timestamp, so skip this field.
	want.CreationInfo.Created = "SKIP ME"
	got.CreationInfo.Created = "SKIP ME"

	// JSON unmarshalling of the SPDX DocElementID is broken.
	// I will file a bug against the open source project.
	//   https://github.com/spdx/tools-golang/blob/main/spdx/common/identifier.go#L100
	cleanID := func(id spdx_common.ElementID) spdx_common.ElementID {
		s := string(id)
		if idx := strings.Index(s, "\""); idx != -1 {
			return spdx_common.ElementID(s[:idx])
		}
		return id
	}
	for _, r := range want.Relationships {
		r.RefA.ElementRefID = cleanID(r.RefA.ElementRefID)
		r.RefB.ElementRefID = cleanID(r.RefB.ElementRefID)
	}
	for _, r := range got.Relationships {
		r.RefA.ElementRefID = cleanID(r.RefA.ElementRefID)
		r.RefB.ElementRefID = cleanID(r.RefB.ElementRefID)
	}

	return want, got
}
