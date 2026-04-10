// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package readme

import (
	"fmt"
	"io/ioutil"
	"path/filepath"
	"strings"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/metrics"
)

const (
	// This project doesn't have a LICENSE file, and so must rely on the
	// license file in the parent directory.
	golibCloudInternalRoot        = "third_party/golibs/vendor/cloud.google.com/go/internal"
	golibCloudInternalLicenseFile = "../LICENSE"
	golibCloudInternalLicenseURL  = "https://fuchsia.googlesource.com/fuchsia/+/refs/heads/main/third_party/golibs/vendor/cloud.google.com/go/LICENSE"

	golibCustomReadme = "tools/check-licenses/assets/readmes/"
)

type golibReadmeBuilder struct {
	url         string
	dir         string
	parent      string
	grandparent string
}

// Create an in-memory representation of a new README.fuchsia file
// by inferring info about the given go library from it's location in the repo.
func NewGolibReadme(path string) (*Readme, error) {
	defer metrics.ReadmeParseDuration.Track()()
	metrics.ReadmeGenerationType.Inc("synthesized_go")

	name := filepath.Base(path)

	var remainder string
	cut := "third_party/golibs/vendor/"
	if _, after, found := strings.Cut(path, cut); found {
		remainder = after
	}
	url := fmt.Sprintf("https://%s", remainder)

	r := &Readme{
		Name:           name,
		URL:            url,
		ProjectRoot:    path,
		ReadmePath:     filepath.Join(golibCustomReadme, path, "README.fuchsia"),
		Licenses:       make([]*ReadmeLicense, 0),
		MalformedLines: make([]string, 0),
	}

	// We need parent and grandparent to generate accurate license URLs
	dir := filepath.Base(path)
	parent := filepath.Base(filepath.Dir(path))
	grandparent := filepath.Base(filepath.Dir(filepath.Dir(path)))

	// Find all license files for this project.
	// They should all live in the root directory of this project.
	directoryContents, err := ioutil.ReadDir(path)
	if err != nil {
		return nil, err
	}
	for _, item := range directoryContents {
		lower := strings.ToLower(item.Name())
		// In practice, all license files for golibs either have "COPYING"
		// or "license" in their name.
		if !(strings.Contains(lower, "licen") ||
			strings.Contains(lower, "copying")) {
			continue
		}

		// There are some instances of go source files that fit the above
		// criteria. Skip those files.
		ext := filepath.Ext(item.Name())
		if ext == ".go" || ext == ".tmpl" || strings.Contains(lower, "template") {
			continue
		}

		licenseUrl, err := getGolibLicenseURL(url, remainder, item.Name(), dir, parent, grandparent)
		if err != nil {
			return nil, err
		}

		r.Licenses = append(r.Licenses, &ReadmeLicense{
			LicenseFile:       item.Name(),
			LicenseFileURL:    licenseUrl,
			LicenseFileFormat: string(singleLicenseFile),
		})
	}

	if path == golibCloudInternalRoot {
		r.Licenses = append(r.Licenses, &ReadmeLicense{
			LicenseFile:       golibCloudInternalLicenseFile,
			LicenseFileURL:    golibCloudInternalLicenseURL,
			LicenseFileFormat: string(singleLicenseFile),
		})
	}

	r.loadLicenseFiles()
	AddReadme(r)
	return r, nil
}

func getGolibLicenseURL(url, remainder, path, dir, parent, grandparent string) (string, error) {
	switch {
	// pkg.go.dev/*
	case parent == "google.golang.org",
		grandparent == "golang.org",
		grandparent == "gonum.org",
		grandparent == "gopkg.in",
		parent == "go.uber.org",
		parent == "gvisor.dev",
		dir == "go.opencensus.io",
		parent == "cloud.google.com",
		parent == "gopkg.in",
		grandparent == "cloud.google.com",
		grandparent == "honnef.co":
		return fmt.Sprintf("https://pkg.go.dev/%s?tab=licenses", remainder), nil

	// github.com/*
	case remainder == "github.com/googleapis/gax-go/v2",
		grandparent == "github.com":
		return fmt.Sprintf("%s/blob/master/%s", url, path), nil

	// Unknown
	default:
		return "", fmt.Errorf("Unknown golib URL for package %s", remainder)
	}
}
