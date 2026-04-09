// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package readme

import (
	"fmt"
	"io/ioutil"
	"path/filepath"
	"strings"

	"github.com/BurntSushi/toml"
)

const (
	rustCrateURLPrefix = "https://fuchsia.googlesource.com/fuchsia"

	rustCrateEmptyRootDir     = "third_party/rust_crates/empty"
	rustCrateEmptyLicenseFile = "../../../../LICENSE"
	rustCrateEmptyLicenseURL  = "https://fuchsia.googlesource.com/fuchsia/+/refs/heads/main/LICENSE"

	rustCrateCustomReadme = "tools/check-licenses/assets/readmes/"
)

type (
	// Represents Cargo.toml files found in rust crates across the repo.
	CargoTomlFile struct {
		Package CargoTomlPackage
	}

	// Represents the "Package" table inside of each Cargo.toml file.
	CargoTomlPackage struct {
		Name       string `toml:"name"`
		Version    string `toml:"version"`
		Repository string `toml:"repository"`
	}
)

// Create an in-memory representation of a new README.fuchsia file
// using data pulled from the Cargo.toml file of the given Rust crate.
func NewRustCrateReadme(path string) (*Readme, error) {
	name := filepath.Base(path)
	parentName := filepath.Base(filepath.Dir(path))
	url := fmt.Sprintf("%s/+/%s/third_party/rust_crates/%s/%s", rustCrateURLPrefix, GitRevision, parentName, name)

	var cargo CargoTomlFile
	_, err := toml.DecodeFile(filepath.Join(path, "Cargo.toml"), &cargo)
	if err != nil {
		// If decoding fails, we fall back to the path-based name and URL
		cargo.Package.Name = name
		cargo.Package.Repository = url
	}

	r := &Readme{
		Name:           cargo.Package.Name,
		URL:            cargo.Package.Repository,
		Version:        cargo.Package.Version,
		ProjectRoot:    path,
		ReadmePath:     filepath.Join(rustCrateCustomReadme, path, "README.fuchsia"),
		Licenses:       make([]*ReadmeLicense, 0),
		MalformedLines: make([]string, 0),
	}

	// Find all license files for this project.
	// They should all live in the root directory of this project.
	directoryContents, err := ioutil.ReadDir(path)
	if err != nil {
		return nil, err
	}
	for _, item := range directoryContents {
		if item.IsDir() && item.Name() == "LICENSES" {
			licensesDir := filepath.Join(path, "LICENSES")
			licensesContents, err := ioutil.ReadDir(licensesDir)
			if err != nil {
				return nil, err
			}
			for _, licenseItem := range licensesContents {
				if licenseItem.IsDir() {
					continue
				}
				licenseRelPath := filepath.Join("LICENSES", licenseItem.Name())
				licenseUrl := fmt.Sprintf("%s/LICENSES/%s", url, licenseItem.Name())
				r.Licenses = append(r.Licenses, &ReadmeLicense{
					LicenseFile:       licenseRelPath,
					LicenseFileURL:    licenseUrl,
					LicenseFileFormat: string(singleLicenseFile),
				})
			}
			continue
		}

		if item.IsDir() {
			continue
		}

		lower := strings.ToLower(item.Name())
		// In practice, all license files for rust projects are either named
		// COPYING or LICENSE
		if !(strings.Contains(lower, "licen") ||
			strings.Contains(lower, "copying")) {
			continue
		}

		// There are some instances of rust source files and template files
		// that fit the above criteria. Skip those files.
		ext := filepath.Ext(item.Name())
		if ext == ".rs" || ext == ".tmpl" || strings.Contains(lower, "template") {
			continue
		}

		licenseUrl := fmt.Sprintf("%s/%s", url, item.Name())
		r.Licenses = append(r.Licenses, &ReadmeLicense{
			LicenseFile:       item.Name(),
			LicenseFileURL:    licenseUrl,
			LicenseFileFormat: string(singleLicenseFile),
		})
	}

	parentPath := filepath.Dir(path)
	if strings.HasSuffix(parentPath, rustCrateEmptyRootDir) {
		r.Licenses = append(r.Licenses, &ReadmeLicense{
			LicenseFile:       rustCrateEmptyLicenseFile,
			LicenseFileURL:    rustCrateEmptyLicenseURL,
			LicenseFileFormat: string(singleLicenseFile),
		})
	}

	r.loadLicenseFiles()
	AddReadme(r)
	return r, nil
}
