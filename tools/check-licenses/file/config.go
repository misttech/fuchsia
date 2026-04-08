// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package file

import (
	"encoding/json"
	"fmt"
	"regexp"
	"sync"

	classifierLib "github.com/google/licenseclassifier/v2"
)

const (
	defaultClassifierThreshold = 0.8
)

var (
	allFilesMu      sync.RWMutex
	AllFiles        map[string]*File
	AllLicenseFiles map[string]*File
	urlRegex        *regexp.Regexp

	spdxFileIndex     int
	spdxFileDataIndex int

	classifier *classifierLib.Classifier
)

func init() {
	AllFiles = make(map[string]*File, 0)
	AllLicenseFiles = make(map[string]*File, 0)
	Config = NewConfig()

}

func Initialize(c *FileConfig) error {
	if c.ClassifierThreshold == 0 {
		c.ClassifierThreshold = defaultClassifierThreshold
	}
	classifier = classifierLib.NewClassifier(c.ClassifierThreshold)
	for _, path := range c.ClassifierLicensePaths {
		err := classifier.LoadLicenses(path)
		if err != nil {
			return fmt.Errorf("Failed to load license texts from path %s: %w", path, err)
		}
	}

	var err error
	urlRegex, err = regexp.Compile(`.*googlesource\.com\/([^\+]+\/)\+.*`)
	if err != nil {
		return err
	}

	// Save the config file to the out directory (if defined).
	if b, err := json.MarshalIndent(c, "", "  "); err != nil {
		return err
	} else {
		plusFile("_config.json", b)
	}

	Config = c
	return nil
}

var Config *FileConfig

type FileConfig struct {
	// Path to the root of the fuchsia repository.
	FuchsiaDir string `json:"fuchsiaDir"`

	// Classifier initialization fields.
	ClassifierThreshold    float64  `json:"classifierThreshold"`
	ClassifierLicensePaths []string `json:"classifierLicensePaths"`

	// Number of bytes to read in to capture copyright information
	// in regular source files.
	CopyrightSize int

	// Some characters in LICENSE texts are being parsed incorrectly.
	// Replace them with their utf8 equivalents so the resulting
	// NOTICE file renders it properly.
	Replacements []*Replacement

	// Extensions map is the list of filetypes that we can expect
	// may have license information included in it.
	Extensions map[string]bool

	// URL overrides can be defined in the config file.
	FileDataURLs []*FileDataURL `json:"urlReplacements"`
}

// Prebuilt libraries have licenses that come from various locations.
// We don't have access to the source URLs for those dependent libraries.
// FileDataURL lets us maintain a separate mapping of library name -> URL,
// which we can use in check-licenses to produce the compliance worksheet.
type FileDataURL struct {
	Source       string            `json:"source"`
	Prefix       string            `json:"prefix"`
	Projects     map[string]bool   `json:"projects"`
	Products     map[string]bool   `json:"products"`
	Boards       map[string]bool   `json:"boards"`
	Replacements map[string]string `json:"replacements"`
}

// Support replacing individual characters with other ones.
// For example, sometimes golang processes ` incorrectly, so we can replace
// instances of that character with ' using Replacement fields in the
// config file.
type Replacement struct {
	Replace string   `json:"replace"`
	With    string   `json:"with"`
	Notes   []string `json:"notes"`
}

func NewConfig() *FileConfig {
	return &FileConfig{
		CopyrightSize: 0,
		Replacements:  make([]*Replacement, 0),
		Extensions:    make(map[string]bool, 0),
		FileDataURLs:  make([]*FileDataURL, 0),
	}
}

func (c *FileConfig) Merge(other *FileConfig) {
	if c.FuchsiaDir == "" {
		c.FuchsiaDir = other.FuchsiaDir
	}

	if c.ClassifierThreshold == 0.0 {
		c.ClassifierThreshold = other.ClassifierThreshold
	}
	c.ClassifierLicensePaths = append(c.ClassifierLicensePaths, other.ClassifierLicensePaths...)

	if c.CopyrightSize == 0 {
		c.CopyrightSize = other.CopyrightSize
	}

	c.Replacements = append(c.Replacements, other.Replacements...)

	if c.Extensions == nil {
		c.Extensions = make(map[string]bool)
	}
	for k, v := range other.Extensions {
		c.Extensions[k] = v
	}

	c.FileDataURLs = append(c.FileDataURLs, other.FileDataURLs...)
}
