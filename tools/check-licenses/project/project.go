// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package project

import (
	"fmt"
	"path/filepath"
	"strings"
	"sync"

	spdx "github.com/spdx/tools-golang/spdx/v2_2"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/file"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/metrics"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/readme"
)

// Project struct follows the format of README.fuchsia files.
// For more info, see the following article:
//
//	https://fuchsia.dev/fuchsia-src/development/source_code/third-party-metadata
type Project struct {
	Root                   string `json:"root"`
	Name                   string `json:"name"`
	URL                    string
	LicenseFiles           []*file.File `licenseFiles`
	RegularFiles           []*file.File
	SearchableRegularFiles []*file.File
	ReadmeFile             *readme.Readme

	// Lock to protect concurrent writes to the file slices below.
	mu sync.Mutex

	// Projects that this project depends on.
	// Constructed from the GN dependency tree.
	Children map[string]*Project

	// SPDX fields
	Package *spdx.Package "json:'package'"
	SPDXID  string        `json:"spdxid"`

	// Compliance fields.
	BeingSurfaced      bool
	SourceCodeIncluded bool
}

// Order implements sort.Interface for []*Project based on the Root field.
type Order []*Project

func (a Order) Len() int           { return len(a) }
func (a Order) Swap(i, j int)      { a[i], a[j] = a[j], a[i] }
func (a Order) Less(i, j int) bool { return a[i].Root < a[j].Root }

// NewProject creates a Project object from a README.fuchsia file.
func NewProject(r *readme.Readme, projectRootPath string) (*Project, error) {
	var err error

	// Make all projectRootPath values relative to Config.FuchsiaDir.
	if strings.Contains(projectRootPath, Config.FuchsiaDir) {
		projectRootPath, err = filepath.Rel(Config.FuchsiaDir, projectRootPath)
		if err != nil {
			return nil, err
		}
	}

	// See if we've already processed this project.
	// If so, return the previously created instance.
	if p, ok := GetProject(projectRootPath); ok {
		metrics.ProjectsProcessed.Inc("cache_hit")
		return p, nil
	}

	p := &Project{
		Root:                   projectRootPath,
		Name:                   r.Name,
		URL:                    r.URL,
		ReadmeFile:             r,
		LicenseFiles:           make([]*file.File, 0),
		RegularFiles:           make([]*file.File, 0),
		SearchableRegularFiles: make([]*file.File, 0),
		Children:               make(map[string]*Project, 0),
	}

	for _, l := range r.Licenses {
		if l.LicenseFile == "" {
			continue
		}
		if l.LicenseFileFormat == "" {
			l.LicenseFileFormat = string(file.SingleLicense)
		}

		path := filepath.Join(Config.FuchsiaDir, p.Root, l.LicenseFile)
		f, err := file.LoadFile(path, file.FileType(l.LicenseFileFormat), r.Name)
		if err != nil {
			// Readmes in asset dirs may occasionally go out of sync with
			// the project they describe. This is to prevent roll breakages.
			if r.IsAssetDirReadme {
				continue
			}

			return nil, fmt.Errorf("failed to load license file %s: %w\n", path, err)
		}
		f.SetURL(l.LicenseFileURL)
		p.LicenseFiles = append(p.LicenseFiles, f)
		l.LicenseFileRef = f
	}

	AddProject(p)
	metrics.ProjectsProcessed.Inc("discovered")

	return p, nil
}

func (p *Project) AddFile(f *file.File) error {
	p.mu.Lock()
	defer p.mu.Unlock()

	if f.FileType() != file.RegularFile {
		// This could be a license file.
		// Make sure this file isn't already in the list.
		for _, l := range p.LicenseFiles {
			if l.RelPath() == f.RelPath() {
				return nil
			}
		}

		p.LicenseFiles = append(p.LicenseFiles, f)

		relPath, _ := filepath.Rel(p.Root, f.RelPath())
		p.ReadmeFile.AddLicense(relPath, f)

		return nil
	}

	p.RegularFiles = append(p.RegularFiles, f)

	ext := filepath.Ext(f.RelPath())
	if _, ok := file.Config.Extensions[ext]; ok {
		p.SearchableRegularFiles = append(p.SearchableRegularFiles, f)
	}

	return nil
}

// GetFiles safely returns a combined list of all regular files and license files.
func (p *Project) GetFiles() []*file.File {
	p.mu.Lock()
	defer p.mu.Unlock()
	files := make([]*file.File, 0, len(p.RegularFiles)+len(p.LicenseFiles))
	files = append(files, p.RegularFiles...)
	files = append(files, p.LicenseFiles...)
	return files
}

// GetLicenseFiles safely returns a list of all license files.
func (p *Project) GetLicenseFiles() []*file.File {
	p.mu.Lock()
	defer p.mu.Unlock()
	files := make([]*file.File, len(p.LicenseFiles))
	copy(files, p.LicenseFiles)
	return files
}

// GetSearchableRegularFiles safely returns a list of all searchable regular files.
func (p *Project) GetSearchableRegularFiles() []*file.File {
	p.mu.Lock()
	defer p.mu.Unlock()
	files := make([]*file.File, len(p.SearchableRegularFiles))
	copy(files, p.SearchableRegularFiles)
	return files
}
