// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package result

import (
	"encoding/json"
	"fmt"
	"io/fs"
	"path/filepath"
	"strings"
	"text/template"
)

type ResultConfig struct {
	FuchsiaDir           string      `json:"fuchsiaDir"`
	Target               string      `json:"target"`
	SPDXDocName          string      `json:"spdxDocName"`
	OutDir               string      `json:"outDir"`
	RootOutDir           string      `json:"rootOutDir"`
	LicenseOutDir        string      `json:"licenseOutDir"`
	Outputs              []string    `json:"outputs"`
	Templates            []*Template `json:"templates"`
	Zip                  bool        `json:"zip"`
	GnGenOutputFile      string      `json:"gnGenOutputFile"`
	OutputLicenseFile    bool        `json:"outputLicenseFile"`
	RunAnalysis          bool        `json:"runAnalysis"`
	OverwriteReadmeFiles bool        `json:"overwriteReadmeFiles"`

	Checks    []*Check `json:"checks"`
	CheckURLs bool

	AllowLists []*AllowList `json:"allowlists"`
}

type Template struct {
	Paths []string `json:"paths"`
	Notes []string `json:"notes"`
}

type AllowList struct {
	Name      string            `json:"name"`
	MatchType string            `json:"matchType"`
	Entries   []*AllowListEntry `json:"entries"`
}

type AllowListEntry struct {
	Projects []string `json:"projects"`
	Bug      string   `json:"bug"`
	Notes    []string `json:"notes"`
}

var (
	AllTemplates map[string]*template.Template
	Config       *ResultConfig
)

func init() {
	AllTemplates = make(map[string]*template.Template)
}

func Initialize(c *ResultConfig) error {
	// Save the config file to the out directory (if defined).
	if b, err := json.MarshalIndent(c, "", "  "); err != nil {
		return err
	} else {
		plusFile("_config.json", b)
	}

	Config = c

	// Ensure no allowlist entries end in a "/"
	for _, check := range c.Checks {
		for k := range check.Allowlist {
			if strings.HasSuffix(k, "/") {
				return fmt.Errorf("\nAllowlist \"%s\" has an entry \"%s\" that ends with \"/\". This is not allowed.\nPlease remove the trailing slash in this allowlist entry.", check.Name, k)
			}
		}
	}

	return initializeTemplates()
}

func initializeTemplates() error {
	for _, templateCategory := range Config.Templates {
		for _, templatePath := range templateCategory.Paths {
			templatePath = filepath.Join(Config.FuchsiaDir, templatePath)
			if err := filepath.WalkDir(templatePath, func(currentPath string, info fs.DirEntry, err error) error {
				if err != nil {
					return err
				}

				if !info.IsDir() {
					if temp, err := template.New(filepath.Base(currentPath)).ParseFiles(currentPath); err != nil {
						return err
					} else {
						relPath, err := filepath.Rel(templatePath, currentPath)
						if err != nil {
							return err
						}
						plusVal(NumInitTemplates, currentPath)
						AllTemplates[relPath] = temp
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

func NewConfig() *ResultConfig {
	return &ResultConfig{
		Outputs:           make([]string, 0),
		Templates:         make([]*Template, 0),
		Checks:            make([]*Check, 0),
		AllowLists:        make([]*AllowList, 0),
		OutputLicenseFile: false,
		RunAnalysis:       false,
	}
}

func (c *ResultConfig) Merge(other *ResultConfig) {
	if c.FuchsiaDir == "" {
		c.FuchsiaDir = other.FuchsiaDir
	}
	if c.Target == "" {
		c.Target = other.Target
	}
	if c.RootOutDir == "" {
		c.RootOutDir = other.RootOutDir
	}
	if c.OutDir == "" {
		c.OutDir = other.OutDir
	}
	if c.LicenseOutDir == "" {
		c.LicenseOutDir = other.LicenseOutDir
	}
	c.Templates = append(c.Templates, other.Templates...)
	c.Outputs = append(c.Outputs, other.Outputs...)
	c.Zip = c.Zip || other.Zip
	if c.GnGenOutputFile == "" {
		c.GnGenOutputFile = other.GnGenOutputFile
	}
	c.OutputLicenseFile = c.OutputLicenseFile || other.OutputLicenseFile
	c.RunAnalysis = c.RunAnalysis || other.RunAnalysis
	if c.SPDXDocName == "" {
		c.SPDXDocName = other.SPDXDocName
	}
	c.Checks = append(c.Checks, other.Checks...)
	c.CheckURLs = c.CheckURLs || other.CheckURLs
	c.OverwriteReadmeFiles = c.OverwriteReadmeFiles || other.OverwriteReadmeFiles

	c.AllowLists = append(c.AllowLists, other.AllowLists...)
}
