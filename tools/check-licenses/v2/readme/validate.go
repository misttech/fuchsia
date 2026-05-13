// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package readme

import (
	"fmt"
	"os"
	"path/filepath"

	v2config "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/config"
)

// Validate checks if the README.fuchsia file structures contain all required fields
// and no unknown fields. It also verifies that referenced paths exist on disk.
// Returns a slice of all encountered errors.
func Validate(fuchsiaDir, readmeFilePath string, readmes []*Readme, config *v2config.MasterConfig) []error {
	var errs []error

	readmeDir := filepath.Dir(readmeFilePath)
	if config != nil && config.OutOfTreeReadmes != nil {
		for logicalPath, physicalPath := range config.OutOfTreeReadmes {
			if filepath.Clean(physicalPath) == filepath.Clean(readmeFilePath) {
				readmeDir = filepath.Join(fuchsiaDir, logicalPath)
				break
			}
		}
		IsProjectBoundary(readmeDir, fuchsiaDir, config.OutOfTreeReadmes)
	}

	for i, r := range readmes {
		var baseDir string
		if i == 0 {
			baseDir = readmeDir
		} else {
			if r.Location != "" {
				baseDir = filepath.Join(fuchsiaDir, r.Location)
				if _, err := os.Stat(baseDir); os.IsNotExist(err) {
					errs = append(errs, fmt.Errorf("Readme %d: 'Location' directory does not exist: %s", i+1, baseDir))
				}
			} else {
				baseDir = readmeDir // Fallback
			}
		}

		relBaseDir, err := filepath.Rel(fuchsiaDir, baseDir)
		if err != nil {
			relBaseDir = baseDir
		}
		if relBaseDir == "." {
			relBaseDir = ""
		}
		allowMissingLicense := false
		if config != nil && config.PolicyExceptions != nil {
			if list, ok := config.PolicyExceptions[v2config.PolicyCheckAllProjectsMustHaveALicense]; ok {
				_, allowMissingLicense = list[relBaseDir]
			}
		}

		// Check 1: Unknown fields
		if len(r.UnknownFields) > 0 {
			errs = append(errs, fmt.Errorf("Readme %d: Found unknown/invalid fields: %+v", i+1, r.UnknownFields))
		}

		// Check 2: Required Fields
		if r.Name == "" {
			errs = append(errs, fmt.Errorf("Readme %d: 'Name' is a required field", i+1))
		}
		if r.URL == "" {
			errs = append(errs, fmt.Errorf("Readme %d: 'URL' is a required field", i+1))
		}
		if r.SecurityCritical != "yes" && r.SecurityCritical != "no" {
			errs = append(errs, fmt.Errorf("Readme %d: 'Security Critical' is required and must be exactly 'yes' or 'no'. Got: %q", i+1, r.SecurityCritical))
		}
		if i > 0 && r.Location == "" {
			errs = append(errs, fmt.Errorf("Readme %d: 'Location' is a required field for sub-projects defined after a DEPENDENCY DIVIDER", i+1))
		}
		if len(r.LicenseFiles) == 0 {
			if !allowMissingLicense {
				errs = append(errs, fmt.Errorf("Readme %d: At least one 'License File' must be specified", i+1))
			}
		} else {
			for _, lf := range r.LicenseFiles {
				if lf.License == "" {
					errs = append(errs, fmt.Errorf("Readme %d: License File '%s' is missing required '  License:' metadata", i+1, lf.Path))
				}
				filePath := filepath.Join(baseDir, lf.Path)
				if _, err := os.Stat(filePath); os.IsNotExist(err) {
					errs = append(errs, fmt.Errorf("Readme %d: License File does not exist: %s", i+1, filePath))
				}
			}
		}

		// Path Existence Checks for Source Files
		for _, sf := range r.SourceFiles {
			filePath := filepath.Join(baseDir, sf.Path)
			if _, err := os.Stat(filePath); os.IsNotExist(err) {
				errs = append(errs, fmt.Errorf("Readme %d: Source File does not exist: %s", i+1, filePath))
			}
		}

		// Path Existence Checks for Non-License Files
		for _, nlf := range r.NonLicenseFiles {
			filePath := filepath.Join(baseDir, nlf.Path)
			if _, err := os.Stat(filePath); os.IsNotExist(err) {
				errs = append(errs, fmt.Errorf("Readme %d: Non-License File does not exist: %s", i+1, filePath))
			}
		}
	}

	return errs
}
