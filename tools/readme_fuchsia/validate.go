// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package readme_fuchsia

import (
	"fmt"
	"os"
	"path/filepath"
)

// Validate checks if the README.fuchsia file structures contain all required fields
// and no unknown fields. It also verifies that referenced paths exist on disk.
func Validate(projectRoot string, readmes []*Readme) []error {
	var errs []error
	var baseDir = projectRoot

	for i, r := range readmes {
		var currentDir string
		if i == 0 {
			currentDir = baseDir
		} else {
			if r.Location != "" {
				currentDir = filepath.Join(baseDir, r.Location)
				if _, err := os.Stat(currentDir); os.IsNotExist(err) {
					errs = append(errs, fmt.Errorf("[%d]: 'Location' directory does not exist: %s (http://go/readme_fuchsia#location)", i+1, r.Location))
				}
			} else {
				currentDir = baseDir // Fallback
			}
		}

		// Check 1: Unknown fields
		if len(r.UnknownFields) > 0 {
			errs = append(errs, fmt.Errorf("[%d]: Found unknown/invalid fields: %+v (http://go/readme_fuchsia#unknown-fields)", i+1, r.UnknownFields))
		}

		// Check 2: Required Fields
		if r.Name == "" {
			errs = append(errs, fmt.Errorf("[%d]: Missing required field 'Name' (http://go/readme_fuchsia#name)", i+1))
		}

		hasUrlAndRev := r.URL != "" && r.Revision != ""
		hasCpeAndVer := r.CPEPrefix != "" && r.Version != ""
		if !hasUrlAndRev && !hasCpeAndVer {
			errs = append(errs, fmt.Errorf("[%d]: Missing required fields. Must specify either ('URL' AND 'Revision') OR ('CPEPrefix' AND 'Version') (http://go/readme_fuchsia#url)", i+1))
		}

		if r.SecurityCritical == "" {
			errs = append(errs, fmt.Errorf("[%d]: Missing required field 'Security Critical' (http://go/readme_fuchsia#security-critical)", i+1))
		} else if r.SecurityCritical != "yes" && r.SecurityCritical != "no" {
			errs = append(errs, fmt.Errorf("[%d]: Field 'Security Critical' has an unknown value. Required 'yes' or 'no', got %q (http://go/readme_fuchsia#security-critical)", i+1, r.SecurityCritical))
		}
		if i > 0 && r.Location == "" {
			errs = append(errs, fmt.Errorf("[%d]: Missing required field 'Location' for sub-project defined after a DEPENDENCY DIVIDER (http://go/readme_fuchsia#location)", i+1))
		}
		if len(r.Licenses) == 0 {
			errs = append(errs, fmt.Errorf("[%d]: Missing required field 'License' (http://go/readme_fuchsia#license)", i+1))
		}
		if len(r.LicenseFiles) == 0 {
			errs = append(errs, fmt.Errorf("[%d]: Missing required field 'License File'. At least one must be specified. (http://go/readme_fuchsia#license-file)", i+1))
		} else {
			for _, lf := range r.LicenseFiles {
				filePath := filepath.Join(currentDir, lf)
				if _, err := os.Stat(filePath); os.IsNotExist(err) {
					errs = append(errs, fmt.Errorf("[%d]: License File does not exist: %s (http://go/readme_fuchsia#license-file)", i+1, filepath.Join(currentDir, lf)))
				}
			}
		}

		// Path Existence Checks for Source Files
		for _, sf := range r.SourceFiles {
			filePath := filepath.Join(currentDir, sf)
			if _, err := os.Stat(filePath); os.IsNotExist(err) {
				errs = append(errs, fmt.Errorf("[%d]: Source File does not exist: %s (http://go/readme_fuchsia#source-file)", i+1, filepath.Join(currentDir, sf)))
			}
		}

		// Path Existence Checks for Non-License Files
		for _, nlf := range r.NonLicenseFiles {
			filePath := filepath.Join(currentDir, nlf)
			if _, err := os.Stat(filePath); os.IsNotExist(err) {
				errs = append(errs, fmt.Errorf("[%d]: Non-License File does not exist: %s (http://go/readme_fuchsia#non-license-file)", i+1, filepath.Join(currentDir, nlf)))
			}
		}
	}

	return errs
}
