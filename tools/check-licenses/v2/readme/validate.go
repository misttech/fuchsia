// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package readme

import (
	"path/filepath"
	"strings"

	v2config "go.fuchsia.dev/fuchsia/tools/check-licenses/v2/config"
	"go.fuchsia.dev/fuchsia/tools/readme_fuchsia"
)

// Validate checks if the README.fuchsia file structures contain all required fields
// and no unknown fields. It also verifies that referenced paths exist on disk.
// Returns a slice of all encountered errors.
func Validate(fuchsiaDir, readmeFilePath string, readmes []*Readme, config *v2config.MasterConfig) []error {
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

	relBaseDir, err := filepath.Rel(fuchsiaDir, readmeDir)
	if err != nil {
		relBaseDir = readmeDir
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

	allowReadmeNeedsUpdate := false
	if config != nil && config.PolicyExceptions != nil {
		if list, ok := config.PolicyExceptions[v2config.CheckNameReadmeFuchsiaNeedsUpdate]; ok {
			_, allowReadmeNeedsUpdate = list[relBaseDir]
		}
	}

	if allowReadmeNeedsUpdate {
		return nil
	}

	errs := readme_fuchsia.Validate(readmeDir, readmes)

	if allowMissingLicense && len(errs) > 0 {
		var filteredErrs []error
		for _, err := range errs {
			msg := err.Error()
			isMissingLicenseErr := strings.Contains(msg, "Missing required field 'License'") || strings.Contains(msg, "Missing required field 'License File'")
			if !isMissingLicenseErr {
				filteredErrs = append(filteredErrs, err)
			}
		}
		errs = filteredErrs
	}

	return errs
}
