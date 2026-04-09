// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package project

import (
	"encoding/json"
	"fmt"
	"io/fs"
	"path/filepath"
	"slices"
	"sync"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/file"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/readme"
)

var Config *ProjectConfig

var (
	RootProject        *Project
	UnknownProject     *Project
	allProjectsMu      sync.RWMutex
	allProjects        map[string]*Project
	filteredProjectsMu sync.RWMutex
	filteredProjects   map[string]*Project

	dedupedLicenseDataMu sync.RWMutex
	dedupedLicenseData   [][]*file.FileData
)

func init() {
	Config = &ProjectConfig{}

	allProjects = make(map[string]*Project, 0)
	filteredProjects = make(map[string]*Project, 0)
	dedupedLicenseData = make([][]*file.FileData, 0)

	UnknownProject = &Project{
		Name:                   "unknown",
		LicenseFiles:           make([]*file.File, 0),
		RegularFiles:           make([]*file.File, 0),
		SearchableRegularFiles: make([]*file.File, 0),
		Children:               make(map[string]*Project, 0),
		ReadmeFile:             &readme.Readme{Licenses: make([]*readme.ReadmeLicense, 0)},
	}
}

// AddProject safely adds a project to the global tracking map.
func AddProject(p *Project) {
	allProjectsMu.Lock()
	defer allProjectsMu.Unlock()
	allProjects[p.Root] = p
}

// GetAllProjects returns a shallow copy of the global projects map.
func GetAllProjects() map[string]*Project {
	allProjectsMu.RLock()
	defer allProjectsMu.RUnlock()

	m := make(map[string]*Project, len(allProjects))
	for k, v := range allProjects {
		m[k] = v
	}
	return m
}

// GetProject safely retrieves a project from the global map by its root path.
func GetProject(root string) (*Project, bool) {
	allProjectsMu.RLock()
	defer allProjectsMu.RUnlock()
	p, ok := allProjects[root]
	return p, ok
}

// AddFilteredProject safely adds a project to the filtered projects map.
func AddFilteredProject(p *Project) {
	filteredProjectsMu.Lock()
	defer filteredProjectsMu.Unlock()
	filteredProjects[p.Root] = p
}

// GetAllFilteredProjects returns a shallow copy of the global filtered projects map.
func GetAllFilteredProjects() map[string]*Project {
	filteredProjectsMu.RLock()
	defer filteredProjectsMu.RUnlock()

	m := make(map[string]*Project, len(filteredProjects))
	for k, v := range filteredProjects {
		m[k] = v
	}
	return m
}

// GetFilteredProject safely retrieves a project from the filtered map by its root path.
func GetFilteredProject(root string) (*Project, bool) {
	filteredProjectsMu.RLock()
	defer filteredProjectsMu.RUnlock()
	p, ok := filteredProjects[root]
	return p, ok
}

// SetDedupedLicenseData safely sets the deduplicated license data.
func SetDedupedLicenseData(data [][]*file.FileData) {
	dedupedLicenseDataMu.Lock()
	defer dedupedLicenseDataMu.Unlock()
	dedupedLicenseData = data
}

// GetDedupedLicenseData safely retrieves the deduplicated license data.
func GetDedupedLicenseData() [][]*file.FileData {
	dedupedLicenseDataMu.RLock()
	defer dedupedLicenseDataMu.RUnlock()
	return dedupedLicenseData
}

func Initialize(c *ProjectConfig) error {
	// Save the config file to the out directory (if defined).
	if b, err := json.MarshalIndent(c, "", "  "); err != nil {
		return err
	} else {
		plusFile("_config.json", b)
	}

	Config = c
	return initializeCustomReadmes()
}

func InitializeForTest() {
	allProjectsMu.Lock()
	allProjects = make(map[string]*Project, 0)
	allProjectsMu.Unlock()

	filteredProjectsMu.Lock()
	filteredProjects = make(map[string]*Project, 0)
	filteredProjectsMu.Unlock()

	dedupedLicenseDataMu.Lock()
	dedupedLicenseData = make([][]*file.FileData, 0)
	dedupedLicenseDataMu.Unlock()
}

// Projects are created using README.fuchsia files.
// Many projects in the fuchsia tree do not have a README.fuchsia file,
// or they are incorrectly formatted.
//
// You can setup custom README.fuchsia files in a special directory,
// point to them in the config file, and they'll be parsed here
// before the rest of check-licenses executes.
func initializeCustomReadmes() error {
	for _, readmeCategory := range Config.Readmes {
		for _, readmePath := range readmeCategory.Paths {
			readmePath = filepath.Join(Config.FuchsiaDir, readmePath)
			if err := filepath.WalkDir(readmePath, func(currentPath string, info fs.DirEntry, err error) error {
				if err != nil {
					return err
				}

				if info.Name() == "README.fuchsia" ||
					info.Name() == "README.chromium" ||
					info.Name() == "README.crashpad" {
					plusVal(NumInitCustomProjects, currentPath)
					projectRoot := filepath.Dir(currentPath)
					projectRoot, err = filepath.Rel(readmePath, projectRoot)
					if err != nil {
						return err
					}

					r, err := readme.NewReadmeFromFileCustomLocation(projectRoot, currentPath)
					if err != nil {
						// Don't error out with these custom README.fuchsia files, so we don't break rollers.
						msg := fmt.Sprintf("Found issue with custom README.fuchsia file: %v: %v\n", currentPath, err)
						plusVal(ReadmeFileInitError, msg)

						return nil
					}

					if _, err := NewProject(r, projectRoot); err != nil {
						msg := fmt.Sprintf("Found issue with NON-custom README.fuchsia file: %v: %v\n", currentPath, err)
						plusVal(ReadmeFileInitError, msg)
						return nil
					}
				}
				return nil
			}); err != nil {
				return err
			}
		}
	}
	return nil
}

type ProjectConfig struct {
	FuchsiaDir string `json:"fuchsiaDir"`
	BuildDir   string `json:"buildDir"`

	GnPath              string `json:"gnPath"`
	GenProjectFile      string `json:"genProjectFile"`
	GenIntermediateFile string `json:"genIntermediateFile"`
	Target              string `json:"target"`

	OutputLicenseFile bool `json:"outputLicenseFile"`

	// Paths to temporary directories holding README.fuchsia files.
	// These files will eventually migrate to their correct locations in
	// the Fuchsia repository.
	Readmes []*Readme `json:"readmes"`

	// Keywords signifying where the license information for one project
	// ends, and the license info for another project begins.
	// (e.g. "third_party")
	Barriers []*Barrier `json:"barriers"`

	// The strings in this map match targets that are found in $root_build_dir/project.json.
	// These targets will be filtered out during project filtering.
	PruneTargets map[string]bool `json:"pruneTargets"`
}

type Readme struct {
	Paths []string `json:"paths"`
	Notes []string `json:"notes"`
}

type Barrier struct {
	Paths      []string `json:"paths"`
	Exceptions []string `json:"exceptions"`
	Notes      []string `json:"notes"`
}

// IsBarrier returns true if the given path is a part of the parent project.
// For example, directories under //third_party are independent projects that
// the parent Fuchsia license file may not apply to.
//
// These "barrier" directories are set in the config file.
func IsBarrier(path string) bool {
	base := filepath.Base(path)
	isBarrier := false
	for _, barrier := range Config.Barriers {
		if slices.Contains(barrier.Paths, base) {
			isBarrier = true
		}
		if slices.Contains(barrier.Exceptions, path) {
			isBarrier = false
		}
	}
	return isBarrier
}

func NewConfig() *ProjectConfig {
	return &ProjectConfig{
		Readmes:      make([]*Readme, 0),
		Barriers:     make([]*Barrier, 0),
		PruneTargets: make(map[string]bool, 0),
	}
}

func (c *ProjectConfig) Merge(other *ProjectConfig) {
	if c.FuchsiaDir == "" {
		c.FuchsiaDir = other.FuchsiaDir
	}
	if c.GnPath == "" {
		c.GnPath = other.GnPath
	}
	if c.GenIntermediateFile == "" {
		c.GenIntermediateFile = other.GenIntermediateFile
	}
	if c.GenProjectFile == "" {
		c.GenProjectFile = other.GenProjectFile
	}
	if c.Target == "" {
		c.Target = other.Target
	}
	if c.BuildDir == "" {
		c.BuildDir = other.BuildDir
	}

	c.Readmes = append(c.Readmes, other.Readmes...)
	c.Barriers = append(c.Barriers, other.Barriers...)
	c.OutputLicenseFile = c.OutputLicenseFile || other.OutputLicenseFile
	c.OutputLicenseFile = c.OutputLicenseFile || other.OutputLicenseFile

	for k, v := range other.PruneTargets {
		c.PruneTargets[k] = v
	}

	// Barrier objects need to be merged together,
	// otherwise the exceptions paths may not work properly.
	mergedBarrier := &Barrier{
		Paths:      make([]string, 0),
		Exceptions: make([]string, 0),
		Notes:      make([]string, 0),
	}
	for _, b := range c.Barriers {
		for _, p := range b.Paths {
			mergedBarrier.Paths = append(mergedBarrier.Paths, p)
		}
		for _, e := range b.Exceptions {
			mergedBarrier.Exceptions = append(mergedBarrier.Exceptions, e)
		}
		for _, n := range b.Notes {
			mergedBarrier.Notes = append(mergedBarrier.Notes, n)
		}
	}
	c.Barriers = []*Barrier{mergedBarrier}
}
