// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package readme

// Readme represents a parsed README.fuchsia file.
type Readme struct {
	Name                     string
	URL                      string
	OriginalURL              string
	Version                  string
	SecurityCritical         string
	Location                 string // The relative path to the sub-project boundary (required for dependencies)
	LicenseFile              string // Legacy flat field, preserved for backward compatibility
	UpstreamGit              string
	Revision                 string
	LicenseAndroidCompatible string
	Description              string
	LocalModifications       string
	Deprecated               string

	// LicenseFiles contains the file-level metadata for multi-license projects.
	// This represents the hierarchical "License File: foo \n -> License: bar" structure.
	LicenseFiles []LicenseEntry

	// SourceFiles contains the file-level metadata for source files with differing licenses.
	// This represents the hierarchical "Source File: foo \n -> License: bar" structure.
	SourceFiles []LicenseEntry

	// UnknownFields captures any "Key: Value" pair that the parser didn't
	// explicitly recognize. This ensures the formatter doesn't delete data,
	// while allowing strict tools (like SHAC) to flag them as errors.
	UnknownFields []UnknownField

	// NonLicenseFiles captures files that the tooling mistakenly flagged as license files.
	// This provides a manual escape hatch for developers.
	NonLicenseFiles []NonLicenseEntry
}

// LicenseEntry represents a single "License File:" entry and its associated
// indented metadata fields (e.g., "-> License:").
type LicenseEntry struct {
	Path             string // The value of "License File:"
	License          string // The value of "  License:" (e.g. "MIT")
	LicenseType      string // The value of "  License Type:" (e.g. "Chromium")
	LicenseFileURL   string // The value of "  License File URL:"
	LicenseReference string // The value of "  License Reference:" (external text pointer)
	Notes            string // The value of "  Notes:"
}

// NonLicenseEntry represents a single "Non-License File:" entry and its explanation.
type NonLicenseEntry struct {
	Path  string // The value of "Non-License File:"
	Notes string // The value of "  Notes:" (legacy alias: Non-License File Explanation)
}

// UnknownField represents an unrecognized Key: Value pair found in the README.
type UnknownField struct {
	Key   string
	Value string
}
