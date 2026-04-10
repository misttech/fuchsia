// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package directory

import (
	"encoding/json"
	"path/filepath"
	"sync"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/metrics"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/project"
)

var Config *DirectoryConfig

var (
	allDirectoriesMu sync.RWMutex
	allDirectories   map[string]*Directory
)

var RootDirectory *Directory

func init() {
	allDirectories = make(map[string]*Directory, 0)
}

func Initialize(c *DirectoryConfig) error {
	// Project readme directories should always be skipped.
	// They are processed during the project Initialize() call.
	readmeSkips := &Skip{
		Paths: []string{},
		Notes: []string{"Always skip project.Readmes paths."},
	}
	for _, r := range project.Config.Readmes {
		readmeSkips.Paths = append(readmeSkips.Paths, r.Paths...)
	}
	c.Skips = append(c.Skips, readmeSkips)

	// check-licenses License pattern directories should also be skipped.
	patternSkips := &Skip{
		Paths: []string{},
		Notes: []string{"Always skip license.PatternRoot paths."},
	}
	c.Skips = append(c.Skips, patternSkips)

	// Save the config file to the out directory (if defined).
	if b, err := json.MarshalIndent(Config, "", "  "); err != nil {
		return err
	} else {
		metrics.AddArtifact("directory/_config.json", b)
	}

	Config = c
	return nil
}

func InitializeForTest() {
	allDirectoriesMu.Lock()
	allDirectories = make(map[string]*Directory)
	allDirectoriesMu.Unlock()
	RootDirectory = nil
}

// AddDirectory safely adds a directory to the global tracking map.
func AddDirectory(d *Directory) {
	allDirectoriesMu.Lock()
	defer allDirectoriesMu.Unlock()
	allDirectories[d.Path] = d
}

// GetAllDirectories returns a shallow copy of the global directories map.
func GetAllDirectories() map[string]*Directory {
	allDirectoriesMu.RLock()
	defer allDirectoriesMu.RUnlock()

	m := make(map[string]*Directory, len(allDirectories))
	for k, v := range allDirectories {
		m[k] = v
	}
	return m
}

type DirectoryConfig struct {
	// FuchsiaDir is the path to the root of your fuchsia workspace.
	// Typically ~/fuchsia, but can be set by environment variables
	// or command-line arguments.
	FuchsiaDir string `json:"fuchsiaDir"`

	// Skips are individual files or directories that should be skipped
	// while traversing the repository.
	Skips []*Skip `json:"skips"`
}

type Skip struct {
	// Paths is a list of strings, describing all of the file paths
	// that should not be processed. Can be individual files or folders.
	Paths []string `json:"paths"`
	// Notes is a freeform text field that will be printed out when this
	// skip entry is exercised during verbose runs of the check-licenses tool.
	Notes []string `json:"notes"`

	// By default, "Paths" entries are full paths relative to $FUCHSIA_DIR.
	// However, paths like ".git" should be skipped everywhere in the fuchsia tree.
	//
	// Set this variable to tell check-licenses that the given paths are *not*
	// relative to $FUCHSIA_DIR, and should be skipped everywhere.
	SkipAnywhere bool `json:"skipAnywhere"`
}

func NewConfig() *DirectoryConfig {
	return &DirectoryConfig{
		Skips: make([]*Skip, 0),
	}
}

func (c *DirectoryConfig) shouldSkip(item string) bool {
	base := filepath.Base(item)
	for _, skip := range c.Skips {
		for _, path := range skip.Paths {
			if item == path {
				return true
			} else if skip.SkipAnywhere && base == path {
				return true
			}
		}
	}
	return false
}

func (c *DirectoryConfig) Merge(other *DirectoryConfig) {
	if c.FuchsiaDir == "" {
		c.FuchsiaDir = other.FuchsiaDir
	}

	c.Skips = append(c.Skips, other.Skips...)
}
