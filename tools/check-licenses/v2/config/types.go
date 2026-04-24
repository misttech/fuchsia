// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package config

import (
	"path/filepath"
	"strings"
)

// MasterConfig is the fully assembled configuration injected into the pipeline stages.
// It is constructed by the ConfigBuilder during the Assembly Phase by merging all
// scattered JSON files from the open-source and proprietary assets directories.
type MasterConfig struct {
	// --- Injected into Discoverer (Stage 1) ---

	// SkipPaths are exact repository paths (relative to fuchsia dir) that the
	// crawler should completely ignore (e.g., "out", "prebuilt/third_party").
	SkipPaths []string

	// SkipAnywhere are basename patterns that should be ignored anywhere in
	// the repository (e.g., ".git", "__pycache__").
	SkipAnywhere []string

	// --- Injected into Grouper (Stage 2) ---

	// BarrierPaths define directories where a new third-party project strictly begins
	// (e.g., "third_party", "prebuilt").
	BarrierPaths []string

	// OutOfTreeReadmes maps a logical project path to the physical path of its
	// README.fuchsia file (stored in tools/check-licenses/assets/readmes/).
	// Key: Logical path (e.g., "third_party/foo")
	// Value: Physical path (e.g., "assets/readmes/third_party/foo/README.fuchsia")
	OutOfTreeReadmes map[string]string

	// --- Injected into Classifier (Stage 4) ---

	// TargetExtensions is a map of file extensions (including the dot, e.g., ".cc")
	// that the classifier should attempt to read and analyze for licenses.
	// Files with unlisted extensions (like .jpg) are skipped during classification
	// to save CPU/memory, but they still exist in the Project struct for compliance reporting.
	TargetExtensions map[string]bool

	// --- Injected into Validator (Stage 5) ---

	// PolicyExceptions maps a Policy Check Name (e.g., "AllProjectsMustHaveALicense") to a set of allowed project paths.
	PolicyExceptions map[string]map[string]RuleMetadata

	// AllowedLicenses maps a highly restricted SPDX ID (e.g., "GPL-2.0", "FTL") to a set of allowed project paths.
	AllowedLicenses map[string]map[string]RuleMetadata

	// ManifestProjectNames maps a project's filesystem path to its name in the manifest.
	// Key: Project path (e.g., "prebuilt/media/firmware/amlogic-decoder")
	// Value: Package name (e.g., "fuchsia_internal/firmware/amlogic-video")
	ManifestProjectNames map[string]string

	// ManifestPrivateProjects tracks if a project path was found in a private manifest.
	ManifestPrivateProjects map[string]bool
}

// IsPrivateProject returns true if the project path belongs to a proprietary/private
// repository. It prevents open-source compliance configs from being contaminated.
func (c *MasterConfig) IsPrivateProject(projectPath string) bool {
	projectPath = filepath.Clean(projectPath)

	// 1. Check if marked private from integration folder
	if c.ManifestPrivateProjects[projectPath] {
		return true
	}

	// 2. Check manifest name prefix
	if name, ok := c.ManifestProjectNames[projectPath]; ok {
		if strings.HasPrefix(name, "fuchsia_internal/") {
			return true
		}
	}

	return false
}

type RuleMetadata struct {
	Bug         string
	Description string
	ConfigPath  string
}

// NewMasterConfig initializes an empty configuration ready to be populated by the builder.
func NewMasterConfig() *MasterConfig {
	return &MasterConfig{
		SkipPaths:               make([]string, 0),
		SkipAnywhere:            make([]string, 0),
		TargetExtensions:        make(map[string]bool),
		BarrierPaths:            make([]string, 0),
		OutOfTreeReadmes:        make(map[string]string),
		PolicyExceptions:        make(map[string]map[string]RuleMetadata),
		AllowedLicenses:         make(map[string]map[string]RuleMetadata),
		ManifestProjectNames:    make(map[string]string),
		ManifestPrivateProjects: make(map[string]bool),
	}
}

// --- JSON File Schemas ---
// These structs define the expected shape of the individual JSON files scattered
// throughout the `assets/configs/` directory. Any JSON file can contain any combination
// of these fields, allowing configuration to be organized by project or by theme.

type ConfigFile struct {
	Includes         []string                    `json:"includes,omitempty"`
	Skips            []SkipEntry                 `json:"skips,omitempty"`
	TargetExtensions *ExtensionEntry             `json:"target_extensions,omitempty"`
	Barriers         []BarrierEntry              `json:"barriers,omitempty"`
	PolicyExceptions map[string][]AllowlistEntry `json:"policy_exceptions,omitempty"`
	AllowedLicenses  map[string][]AllowlistEntry `json:"allowed_licenses,omitempty"`
}

type SkipEntry struct {
	Bug          string   `json:"bug,omitempty"`
	Description  string   `json:"description,omitempty"`
	Paths        []string `json:"paths"`
	SkipAnywhere bool     `json:"skipAnywhere,omitempty"`
}

type ExtensionEntry struct {
	Description string   `json:"description,omitempty"`
	Extensions  []string `json:"extensions"` // E.g., [".cc", ".cpp", ".h"]
}

type BarrierEntry struct {
	Bug         string   `json:"bug,omitempty"`
	Description string   `json:"description,omitempty"`
	Paths       []string `json:"paths"`
}

type AllowlistEntry struct {
	Bug         string   `json:"bug,omitempty"`
	Description string   `json:"description,omitempty"`
	Paths       []string `json:"paths"` // Paths to allowed projects/files
}
