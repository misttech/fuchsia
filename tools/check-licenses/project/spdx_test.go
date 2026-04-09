// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package project

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	spdx_common "github.com/spdx/tools-golang/spdx/common"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/file"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/readme"
)

// =========================================================================
// SPDX Package Generation Tests
// =========================================================================

// TestGenerateSPDXPackage_Identity verifies that the SPDX Package is generated
// with the correct deterministic SPDXID and required default NOASSERTION fields.
func TestGenerateSPDXPackage_Identity(t *testing.T) {
	tempDir := setup(t)
	root := filepath.Join(tempDir, "src", "foo")
	os.MkdirAll(root, 0755)

	r := &readme.Readme{Name: "FooProject"}
	p, err := NewProject(r, "src/foo")
	if err != nil {
		t.Fatal(err)
	}

	pkg, err := p.GenerateSPDXPackage()
	if err != nil {
		t.Fatal(err)
	}

	if pkg.PackageName != "FooProject" {
		t.Errorf("Expected PackageName to be 'FooProject', got %q", pkg.PackageName)
	}
	if !strings.HasPrefix(string(pkg.PackageSPDXIdentifier), "Package-") {
		t.Errorf("Expected SPDXIdentifier to start with 'Package-', got %q", pkg.PackageSPDXIdentifier)
	}
	if pkg.PackageLicenseConcluded != "NOASSERTION" {
		t.Errorf("Expected PackageLicenseConcluded to default to 'NOASSERTION', got %q", pkg.PackageLicenseConcluded)
	}
	if pkg.PackageVerificationCode.Value != "0" {
		t.Errorf("Expected PackageVerificationCode to be '0', got %q", pkg.PackageVerificationCode.Value)
	}
}

// TestSPDXExpression_NoLicenses verifies that a project with zero license files
// correctly outputs NOASSERTION for its concluded license.
func TestSPDXExpression_NoLicenses(t *testing.T) {
	tempDir := setup(t)
	root := filepath.Join(tempDir, "src", "nolicenses")
	os.MkdirAll(root, 0755)

	p, _ := NewProject(&readme.Readme{Name: "NoLicenses"}, "src/nolicenses")
	pkg, _ := p.GenerateSPDXPackage()

	if pkg.PackageLicenseConcluded != "NOASSERTION" {
		t.Errorf("Expected NOASSERTION, got %q", pkg.PackageLicenseConcluded)
	}
}

// TestSPDXExpression_SingleLicense_SingleData verifies the SPDX expression format
// for a standard project with one license file containing one license text.
func TestSPDXExpression_SingleLicense_SingleData(t *testing.T) {
	tempDir := setup(t)
	root := filepath.Join(tempDir, "src", "single_single")
	os.MkdirAll(root, 0755)

	licensePath := filepath.Join(root, "LICENSE")
	os.WriteFile(licensePath, []byte("MIT License"), 0644)

	f, _ := file.LoadFile(licensePath, file.SingleLicense, "SingleSingle")

	p, _ := NewProject(&readme.Readme{Name: "SingleSingle"}, "src/single_single")
	p.LicenseFiles = append(p.LicenseFiles, f)

	pkg, _ := p.GenerateSPDXPackage()

	if !strings.HasPrefix(pkg.PackageLicenseConcluded, "(") || !strings.HasSuffix(pkg.PackageLicenseConcluded, ")") {
		t.Errorf("Expected expression to be wrapped in parentheses, got %q", pkg.PackageLicenseConcluded)
	}
	if strings.Contains(pkg.PackageLicenseConcluded, " AND ") {
		t.Errorf("Expected no AND operators for a single data segment, got %q", pkg.PackageLicenseConcluded)
	}
}

// TestSPDXExpression_SingleLicense_MultiData verifies the SPDX expression format
// for a project with one license file containing multiple license texts (e.g. Chromium NOTICE).
func TestSPDXExpression_SingleLicense_MultiData(t *testing.T) {
	tempDir := setup(t)
	root := filepath.Join(tempDir, "src", "single_multi")
	os.MkdirAll(root, 0755)

	noticePath := filepath.Join(root, "NOTICE")
	content := []byte("\n--------------------\nlibraryA\n--------------------\nlicense A\n--------------------\nlibraryB\n--------------------\nlicense B\n")
	os.WriteFile(noticePath, content, 0644)

	f, err := file.LoadFile(noticePath, file.MultiLicenseChromium, "SingleMulti")
	if err != nil {
		t.Fatal(err)
	}

	p, _ := NewProject(&readme.Readme{Name: "SingleMulti"}, "src/single_multi")
	p.LicenseFiles = append(p.LicenseFiles, f)

	pkg, _ := p.GenerateSPDXPackage()

	if !strings.Contains(pkg.PackageLicenseConcluded, " AND ") {
		t.Errorf("Expected expression to contain AND operator joining the multiple data segments, got %q", pkg.PackageLicenseConcluded)
	}
	if strings.Count(pkg.PackageLicenseConcluded, "(") != 1 {
		t.Errorf("Expected exactly 1 set of parentheses for a single file, got %q", pkg.PackageLicenseConcluded)
	}
}

// TestSPDXExpression_MultiLicense verifies the SPDX expression format for a project
// with multiple distinct license files.
func TestSPDXExpression_MultiLicense(t *testing.T) {
	tempDir := setup(t)
	root := filepath.Join(tempDir, "src", "multi")
	os.MkdirAll(root, 0755)

	path1 := filepath.Join(root, "LICENSE1")
	os.WriteFile(path1, []byte("License 1"), 0644)
	f1, _ := file.LoadFile(path1, file.SingleLicense, "Multi")

	path2 := filepath.Join(root, "LICENSE2")
	os.WriteFile(path2, []byte("License 2"), 0644)
	f2, _ := file.LoadFile(path2, file.SingleLicense, "Multi")

	p, _ := NewProject(&readme.Readme{Name: "Multi"}, "src/multi")
	p.LicenseFiles = append(p.LicenseFiles, f1, f2)

	pkg, _ := p.GenerateSPDXPackage()

	if !strings.Contains(pkg.PackageLicenseConcluded, ") AND (") {
		t.Errorf("Expected expression to join multiple files with AND between parenthesis groups, got %q", pkg.PackageLicenseConcluded)
	}
}

// TestSPDXExpression_Prebuilt verifies the special prebuilt edge case where
// the top-level file SPDXID is used instead of attempting to parse FileData.
func TestSPDXExpression_Prebuilt(t *testing.T) {
	tempDir := setup(t)
	root := filepath.Join(tempDir, "prebuilt", "third_party", "foo")
	os.MkdirAll(root, 0755)

	licensePath := filepath.Join(root, "LICENSE")
	os.WriteFile(licensePath, []byte("Prebuilt License"), 0644)

	f, _ := file.LoadFile(licensePath, file.SingleLicense, "Prebuilt")
	expectedID := spdx_common.ElementID(f.SPDXID())

	p, _ := NewProject(&readme.Readme{Name: "Prebuilt"}, "prebuilt/third_party/foo")
	p.LicenseFiles = append(p.LicenseFiles, f)

	pkg, _ := p.GenerateSPDXPackage()

	if !strings.Contains(pkg.PackageLicenseConcluded, string(expectedID)) {
		t.Errorf("Expected prebuilt expression to use the file's SPDXID %q directly, got %q", expectedID, pkg.PackageLicenseConcluded)
	}
}
