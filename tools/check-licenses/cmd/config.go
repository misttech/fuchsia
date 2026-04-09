// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"os"
	"path/filepath"
	"regexp"
	"strings"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/directory"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/file"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/project"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/result"
)

// The Include struct contains information about paths and files
// that should be included & merged into the active config file.
type Include struct {
	// Path to the config.json file or root directory.
	Path []string `json:"paths"`
	// When true, recursively find all *.json files starting at the path
	// directory. Attempt to parse and include all found files
	// as config files.
	Recursive bool `json:"recursive"`
	// A simple comment field, used to explain what the given config file
	// should be used for / why it is being included.
	Notes []string `json:"notes"`
	// When true, check-licenses will fail if the path is unavailable.
	// Defaults to false, and allows us to attempt to load in config files
	// from other repositories, but continue with execution if those
	// repos are not available on your local machine.
	Required bool `json:"required"`
}

type CheckLicensesConfig struct {
	// Includes defines a list of files or directories that contain
	// config.json files. This allows check-licenses configuration details
	// to be spread out across the fuchsia workspace.
	Includes []Include `json:"includes"`

	// LogLevel controls how much output is printed to stdout.
	// See log.go for more information.
	LogLevel int `json:"logLevel"`

	// Flag stating whether or not check-licenses should generate
	// a NOTICE file.
	OutputLicenseFile bool `json:"outputLicenseFile"`

	// Flag stating whether or not check-licenses should analyze licenses
	// and run tests on the output results
	RunAnalysis bool `json:"runAnalysis"`

	// FuchsiaDir is the path to the root of your fuchsia workspace.
	// Typically ~/fuchsia, but can be set by environment variables
	// or command-line arguments.
	FuchsiaDir string `json:"fuchsiaDir"`
	// OutDir is the path to the output directory for your GN workspace.
	// Typically ~/fuchsia/out/default, but can be set by environment variables
	// or command-line arguments.
	OutDir string `json:"outDir"`

	// On the command-line, a user can provide a GN target (e.g. //sdk)
	// to generate a NOTICE file for.
	Target string `json:"target"`

	// The following variables represent Config files for the
	// check-licenses subpackage of the same name.
	File      *file.FileConfig           `json:"file"`
	Project   *project.ProjectConfig     `json:"project"`
	Directory *directory.DirectoryConfig `json:"directory"`
	Result    *result.ResultConfig       `json:"result"`
}

// Create a new CheckLicensesConfig object by reading in a config.json file.
func NewCheckLicensesConfig(path string, configVars map[string]string) (*CheckLicensesConfig, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("Failed to read config file [%v]: %w\n", path, err)
	}

	c, err := NewCheckLicensesConfigJson(string(b), configVars)
	if err != nil {
		return nil, fmt.Errorf("Failed to parse JSON config file [%v]: %v\n", path, err)
	}
	return c, nil
}

// Create a new CheckLicensesConfig object by consuming a json config string.
func NewCheckLicensesConfigJson(configJson string, configVars map[string]string) (*CheckLicensesConfig, error) {
	for k, v := range configVars {
		configJson = strings.ReplaceAll(configJson, k, v)
	}

	// Make sure all variables have been replaced.
	r := regexp.MustCompile(`({[\w]+})`)
	matches := r.FindAllStringSubmatch(configJson, -1)

	if len(matches) > 0 {
		return nil, fmt.Errorf("Found unexpanded variable(s) in config file: %v\n", configJson)
	}

	c := &CheckLicensesConfig{
		File:      file.NewConfig(),
		Project:   project.NewConfig(),
		Directory: directory.NewConfig(),
		Result:    result.NewConfig(),
	}

	d := json.NewDecoder(strings.NewReader(configJson))
	// TODO: Readme processing updates. Soft Transition
	//d.DisallowUnknownFields()
	if err := d.Decode(c); err != nil {
		return nil, err
	}

	if err := c.ProcessIncludes(configVars); err != nil {
		return nil, err
	}

	return c, nil
}

// Merge two CheckLicenseConfig objects together.
func (c *CheckLicensesConfig) Merge(other *CheckLicensesConfig) error {
	c.Includes = append(c.Includes, other.Includes...)

	if other.LogLevel > c.LogLevel {
		c.LogLevel = other.LogLevel
	}

	c.OutputLicenseFile = c.OutputLicenseFile || other.OutputLicenseFile
	c.RunAnalysis = c.RunAnalysis || other.RunAnalysis

	if c.FuchsiaDir == "" {
		c.FuchsiaDir = other.FuchsiaDir
	}
	if c.OutDir == "" {
		c.OutDir = other.OutDir
	}

	if c.Target == "" {
		c.Target = other.Target
	}

	c.File.Merge(other.File)
	c.Project.Merge(other.Project)
	c.Directory.Merge(other.Directory)
	c.Result.Merge(other.Result)

	return nil
}

// Process each "include" entry in this config file.
func (c *CheckLicensesConfig) ProcessIncludes(configVars map[string]string) error {
	// Loop over the Includes field and merge in all config files
	// from that list, recursively.
	if len(c.Includes) > 0 {
		for _, include := range c.Includes {
			if err := c.processInclude(&include, configVars); err != nil {
				return err
			}
		}
	}

	return nil
}

func (c *CheckLicensesConfig) processInclude(include *Include, configVars map[string]string) error {
	// Process a single Config include file.
	processPath := func(path string) error {
		c2, err := NewCheckLicensesConfig(path, configVars)
		// If we get an error loading the config file,
		// it may be because a given submodule isn't
		// available on your machine (e.g. //vendor/google).
		//
		// Only error out if this config section is marked
		// as "required".
		if err != nil {
			if errors.Is(err, os.ErrNotExist) && !include.Required {
				if c.LogLevel > 0 {
					log.Printf("Failed to create config file for %s: %v.\n",
						path, err)
				}
				return nil
			} else {
				return err
			}
		}
		c.Merge(c2)
		return nil
	}

	// Process a Config include directory, recursively.
	// Only attempt to parse explicit .json files to prevent crashes
	// on READMEs, .gitignore, or other non-json files.
	processRecursive := func(path string, info os.FileInfo, err error) error {
		if !info.IsDir() && filepath.Ext(path) == ".json" {
			if err := processPath(path); err != nil {
				return err
			}
		}
		return nil
	}

	for _, path := range include.Path {
		if include.Recursive {
			if err := filepath.Walk(path, processRecursive); err != nil {
				return err
			}
		} else {
			if err := processPath(path); err != nil {
				return err
			}
		}
	}
	return nil
}
