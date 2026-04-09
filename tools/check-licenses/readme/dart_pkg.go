// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package readme

import (
	"fmt"
	"io/ioutil"
	"path/filepath"
	"strings"
)

const (
	dartPkgCustomReadme = "tools/check-licenses/assets/readmes/"
)

// Create an in-memory representation of a new README.fuchsia file
// by inferring info about a Dart package given it's location in the repo.
func NewDartPkgReadme(path string) (*Readme, error) {
	name := filepath.Base(path)
	url := fmt.Sprintf("https://pub.dev/packages/%s", name)

	r := &Readme{
		Name:           name,
		URL:            url,
		ProjectRoot:    path,
		ReadmePath:     filepath.Join(dartPkgCustomReadme, path, "README.fuchsia"),
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
		lower := strings.ToLower(item.Name())
		// In practice, all license files for dart packages either have "COPYING"
		// or "license" in their name.
		if !(strings.Contains(lower, "licen") ||
			strings.Contains(lower, "copying")) {
			continue
		}

		// There are some instances of dart source files and template files
		// that fit the above criteria. Skip those files.
		ext := filepath.Ext(item.Name())
		if ext == ".dart" || ext == ".tmpl" || strings.Contains(lower, "template") {
			continue
		}

		licenseUrl := fmt.Sprintf("%s/license", url)
		r.Licenses = append(r.Licenses, &ReadmeLicense{
			LicenseFile:       item.Name(),
			LicenseFileURL:    licenseUrl,
			LicenseFileFormat: string(singleLicenseFile),
		})
	}

	r.loadLicenseFiles()
	AddReadme(r)
	return r, nil
}
